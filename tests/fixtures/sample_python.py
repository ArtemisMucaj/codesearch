"""A sample Python file for testing the parser."""

from typing import List, Optional


class User:
    """Represents a user in the system."""

    def __init__(self, user_id: int, name: str, email: str):
        self.user_id = user_id
        self.name = name
        self.email = email

    def display_name(self) -> str:
        """Get the user's display name."""
        return self.name

    def is_valid(self) -> bool:
        """Check if the user data is valid."""
        return bool(self.name) and "@" in self.email


class Calculator:
    """A simple calculator class."""

    def __init__(self):
        self.result = 0

    def add(self, value: int) -> "Calculator":
        """Add a value to the result."""
        self.result += value
        return self

    def subtract(self, value: int) -> "Calculator":
        """Subtract a value from the result."""
        self.result -= value
        return self

    def get_result(self) -> int:
        """Get the current result."""
        return self.result


def calculate_sum(numbers: List[int]) -> int:
    """Calculate the sum of a list of numbers."""
    return sum(numbers)


def find_max(numbers: List[int]) -> Optional[int]:
    """Find the maximum value in a list."""
    if not numbers:
        return None
    return max(numbers)


def validate_email(email: str) -> bool:
    """Validate an email address format."""
    return "@" in email and "." in email.split("@")[-1]


if __name__ == "__main__":
    user = User(1, "Alice", "alice@example.com")
    print(f"User: {user.display_name()}")
    print(f"Valid: {user.is_valid()}")
