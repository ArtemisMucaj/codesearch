/**
 * A sample JavaScript file for testing the parser.
 */

/**
 * User class representing a system user.
 */
class User {
  constructor(id, name, email) {
    this.id = id;
    this.name = name;
    this.email = email;
  }

  /**
   * Get the user's display name.
   */
  displayName() {
    return this.name;
  }

  /**
   * Check if the user data is valid.
   */
  isValid() {
    return Boolean(this.name) && typeof this.email === "string" && this.email.includes("@");
  }
}

/**
 * Calculate the sum of two numbers.
 */
function add(a, b) {
  return a + b;
}

/**
 * Calculate the product of two numbers.
 */
function multiply(a, b) {
  return a * b;
}

/**
 * Validate an email address.
 */
const validateEmail = (email) => {
  const regex = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;
  return regex.test(email);
};

/**
 * Fetch user data from API.
 */
const fetchUser = async (userId) => {
  const response = await fetch(`/api/users/${userId}`);
  return response.json();
};

/**
 * Process an array of items.
 */
const processItems = (items, callback) => {
  return items.map(callback);
};

module.exports = {
  User,
  add,
  multiply,
  validateEmail,
  fetchUser,
  processItems,
};
