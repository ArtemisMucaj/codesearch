/**
 * A sample Express.js router for testing CommonJS require() import tracking.
 * Imports the middleware from sample_middleware.js under a local alias so we
 * can verify that `const alias = require(...)` is captured as an Import edge
 * in the call graph even when the alias differs from the exported function name.
 */

const express = require('express');
const addSource = require('./sample_middleware.js');

const router = express.Router();

function setupApiRoutes(app) {
  router.use(addSource);
  app.use('/api', router);
}

module.exports = setupApiRoutes;
