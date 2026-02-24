/**
 * A sample Express.js middleware for testing CommonJS require() import tracking.
 * Attaches the calling application's name (from the X-App-Name header) to the
 * request object so downstream handlers can use it.
 */

function appApplicationSource(req, res, next) {
  const appName = req.get('X-App-Name');
  if (!appName) {
    return res.status(400).json({ error: 'Missing X-App-Name header' });
  }
  req.appSource = appName;
  next();
}

module.exports = appApplicationSource;
