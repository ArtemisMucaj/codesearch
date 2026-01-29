use std::collections::HashMap;

pub struct User {
    pub id: u64,
    pub name: String,
    pub email: String,
}

impl User {
    pub fn new(id: u64, name: String, email: String) -> Self {
        Self { id, name, email }
    }

    pub fn display_name(&self) -> &str {
        &self.name
    }
}

pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

pub trait Validate {
    fn is_valid(&self) -> bool;
}

impl Validate for User {
    fn is_valid(&self) -> bool {
        !self.name.is_empty() && self.email.contains('@')
    }
}

#[derive(Debug, Clone)]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Failed(String),
}

pub const MAX_CONNECTIONS: usize = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn test_user_validation() {
        let user = User::new(1, "Alice".to_string(), "alice@example.com".to_string());
        assert!(user.is_valid());
    }
}
