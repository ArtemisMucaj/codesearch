//! Integration tests for LLM community naming: cache miss → LLM → cache write,
//! then cache hit on a second pass (no further LLM calls). Uses an in-memory
//! DuckDB analysis repository and a scripted chat client, so no network or model
//! is required.

use std::sync::Arc;

use async_trait::async_trait;
use duckdb::Connection;
use tokio::sync::Mutex;

use codesearch::{
    stable_community_id, ChatClient, Cluster, CommunityNamingUseCase, DomainError,
    DuckdbAnalysisRepository, NamingRegistry,
};

/// Chat client that returns a canned name and counts how many times it is asked.
struct CountingChatClient {
    reply: String,
    calls: Mutex<usize>,
}

impl CountingChatClient {
    fn new(reply: &str) -> Self {
        Self {
            reply: reply.to_string(),
            calls: Mutex::new(0),
        }
    }

    async fn call_count(&self) -> usize {
        *self.calls.lock().await
    }
}

#[async_trait]
impl ChatClient for CountingChatClient {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, DomainError> {
        *self.calls.lock().await += 1;
        Ok(self.reply.clone())
    }

    async fn complete_json(
        &self,
        system: &str,
        user: &str,
        _schema_name: &str,
        _schema: &serde_json::Value,
    ) -> Result<String, DomainError> {
        // Return the name wrapped in the structured shape the use case expects.
        let _ = (system, user);
        *self.calls.lock().await += 1;
        Ok(format!(r#"{{"name": "{}"}}"#, self.reply))
    }
}

fn cluster(members: &[&str]) -> Cluster {
    let members: Vec<String> = members.iter().map(|s| s.to_string()).collect();
    Cluster {
        id: stable_community_id("c", &members),
        display_name: None,
        repository_id: "repo".to_string(),
        dominant_language: "php".to_string(),
        size: members.len(),
        cohesion: 0.5,
        members,
    }
}

async fn in_memory_repo() -> Arc<DuckdbAnalysisRepository> {
    let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
    Arc::new(
        DuckdbAnalysisRepository::with_connection(conn)
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn names_clusters_and_caches_by_id() {
    let repo = in_memory_repo().await;
    let chat = CountingChatClient::new("Camera Event Models");
    let naming = CommunityNamingUseCase::new(repo.clone(), Arc::new(NamingRegistry::new()));

    let mut clusters = vec![cluster(&["src/models/events/Camera.php"])];

    // First pass: cache miss → LLM fills display_name.
    naming.name_clusters(&mut clusters, &chat).await;
    assert_eq!(
        clusters[0].display_name.as_deref(),
        Some("Camera Event Models")
    );
    assert_eq!(chat.call_count().await, 1);

    // Second pass over a *fresh* cluster with the same membership (hence the same
    // stable id): the name is served from the cache, so the LLM is not called
    // again.
    let mut clusters2 = vec![cluster(&["src/models/events/Camera.php"])];
    naming.name_clusters(&mut clusters2, &chat).await;
    assert_eq!(
        clusters2[0].display_name.as_deref(),
        Some("Camera Event Models")
    );
    assert_eq!(
        chat.call_count().await,
        1,
        "cache should have prevented a second LLM call"
    );
}

#[tokio::test]
async fn cache_persists_across_use_case_instances() {
    let repo = in_memory_repo().await;
    let chat = CountingChatClient::new("Heating Control");

    let members = ["src/heating/Pid.php", "src/heating/Curve.php"];
    // Shared, as the container shares it: a fresh use case is built per call, so
    // the registry has to outlive any one of them.
    let registry = Arc::new(NamingRegistry::new());

    // Name once through one use-case instance.
    {
        let naming = CommunityNamingUseCase::new(repo.clone(), Arc::clone(&registry));
        let mut clusters = vec![cluster(&members)];
        naming.name_clusters(&mut clusters, &chat).await;
        assert_eq!(clusters[0].display_name.as_deref(), Some("Heating Control"));
    }
    assert_eq!(chat.call_count().await, 1);

    // A brand-new use-case instance backed by the same repo reuses the cached
    // name (the id is stable, so the row is found).
    {
        let naming = CommunityNamingUseCase::new(repo.clone(), Arc::clone(&registry));
        let mut clusters = vec![cluster(&members)];
        naming.name_clusters(&mut clusters, &chat).await;
        assert_eq!(clusters[0].display_name.as_deref(), Some("Heating Control"));
    }
    assert_eq!(
        chat.call_count().await,
        1,
        "second instance must hit the cache"
    );
}

/// The regression this guards: naming only reaches the cache once a run has
/// generated its names, so a client polling for them — or switching between the
/// graph and cluster views — issues several requests inside that window. Without
/// a shared in-flight registry each one re-names the same communities, and the
/// LLM call count scales with the number of concurrent requests.
#[tokio::test]
async fn concurrent_runs_do_not_rename_the_same_communities() {
    let repo = in_memory_repo().await;
    let chat = SlowCountingChatClient::new("Device Provisioning");
    let registry = Arc::new(NamingRegistry::new());

    let members = ["src/provisioning/Enroll.php", "src/provisioning/Claim.php"];

    // Two runs over the same community, started together — the shape of a client
    // that polls while the first response is still in flight.
    let first = {
        let (repo, registry) = (repo.clone(), Arc::clone(&registry));
        let chat = &chat;
        async move {
            let naming = CommunityNamingUseCase::new(repo, registry);
            let mut clusters = vec![cluster(&members)];
            naming.name_clusters(&mut clusters, chat).await;
            clusters
        }
    };
    let second = {
        let (repo, registry) = (repo.clone(), Arc::clone(&registry));
        let chat = &chat;
        async move {
            let naming = CommunityNamingUseCase::new(repo, registry);
            let mut clusters = vec![cluster(&members)];
            naming.name_clusters(&mut clusters, chat).await;
            clusters
        }
    };
    let (a, b) = tokio::join!(first, second);

    assert_eq!(
        chat.call_count().await,
        1,
        "the community must be named once, not once per concurrent request"
    );

    // Exactly one run owns the claim and returns the name; the other leaves the
    // id in place. Its next request reads the name from the cache.
    let named = [&a, &b]
        .iter()
        .filter(|c| c[0].display_name.is_some())
        .count();
    assert_eq!(named, 1, "one run names, the other defers to it");
}

/// A chat client that holds each call open long enough for a concurrent run to
/// observe it as in flight. Without the delay both runs could finish their
/// generation before either checks the registry, and the test would pass whether
/// or not the dedup works.
struct SlowCountingChatClient {
    reply: String,
    calls: Mutex<usize>,
}

impl SlowCountingChatClient {
    fn new(reply: &str) -> Self {
        Self {
            reply: reply.to_string(),
            calls: Mutex::new(0),
        }
    }

    async fn call_count(&self) -> usize {
        *self.calls.lock().await
    }
}

#[async_trait]
impl ChatClient for SlowCountingChatClient {
    async fn complete(&self, _system: &str, _user: &str) -> Result<String, DomainError> {
        *self.calls.lock().await += 1;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(self.reply.clone())
    }

    async fn complete_json(
        &self,
        _system: &str,
        _user: &str,
        _schema_name: &str,
        _schema: &serde_json::Value,
    ) -> Result<String, DomainError> {
        *self.calls.lock().await += 1;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(format!(r#"{{"name": "{}"}}"#, self.reply))
    }
}
