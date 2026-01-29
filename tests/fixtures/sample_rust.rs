/// A sample Rust file for testing the parser.

use std::collections::HashMap;

/// A simple struct representing a user.
pub struct User {
    pub id: u64,
    pub name: String,
    pub email: String,
}

impl User {
    /// Create a new user.
    pub fn new(id: u64, name: String, email: String) -> Self {
        Self { id, name, email }
    }

    /// Get the user's display name.
    pub fn display_name(&self) -> &str {
        &self.name
    }
}

/// Calculate the sum of two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

/// Calculate the product of two numbers.
fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

/// A trait for objects that can be validated.
pub trait Validate {
    fn is_valid(&self) -> bool;
}

impl Validate for User {
    fn is_valid(&self) -> bool {
        !self.name.is_empty() && self.email.contains('@')
    }
}

/// Status of an operation.
#[derive(Debug, Clone)]
pub enum Status {
    Pending,
    InProgress,
    Completed,
    Failed(String),
}

/// Application configuration.
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
