const fs = require('fs');
const path = require('path');

function loadConfig() {
  const configPath = process.env.AGENTRAAS_CONFIG_PATH || path.join(__dirname, '..', '..', 'config', 'services.json');
  
  if (!fs.existsSync(configPath)) {
    throw new Error(`Config file not found: ${configPath}`);
  }
  
  const raw = fs.readFileSync(configPath, 'utf8');
  const config = JSON.parse(raw);
  
  // Validate basic structure
  for (const [serviceName, serviceConfig] of Object.entries(config)) {
    if (!serviceConfig.baseUrl) {
      throw new Error(`Service ${serviceName}: missing baseUrl`);
    }
    if (!serviceConfig.actions || Object.keys(serviceConfig.actions).length === 0) {
      throw new Error(`Service ${serviceName}: no actions defined`);
    }
  }
  
  return config;
}

function buildServiceRoutes(config) {
  const routes = {};
  
  for (const [serviceName, serviceConfig] of Object.entries(config)) {
    for (const [actionName, actionConfig] of Object.entries(serviceConfig.actions)) {
      const routeKey = `${serviceName}.${actionName}`;

      // `authHeader` can be legitimately null (e.g. zapier — the webhook URL
      // itself carries the secret, no header wanted). `||` treats null the
      // same as "not specified" and silently falls back to 'Authorization',
      // which added an unwanted header to every zapier call. Only default
      // when the key is genuinely absent from the config.
      const authHeader = Object.prototype.hasOwnProperty.call(serviceConfig, 'authHeader')
        ? serviceConfig.authHeader
        : 'Authorization';

      routes[routeKey] = {
        method: actionConfig.method,
        url: `${serviceConfig.baseUrl}${actionConfig.path}`,
        internal: serviceConfig.internal || false,
        authType: serviceConfig.authType || 'bearer',
        authHeader,
        contentType: serviceConfig.contentType || 'application/json',
        extraHeaders: serviceConfig.extraHeaders || null,
        validation: actionConfig.validation || {}
      };
    }
  }
  
  return routes;
}

function getValidationRules(config) {
  const rules = [];
  
  for (const [serviceName, serviceConfig] of Object.entries(config)) {
    for (const [actionName, actionConfig] of Object.entries(serviceConfig.actions)) {
      if (actionConfig.validation && Object.keys(actionConfig.validation).length > 0) {
        rules.push({
          service: serviceName,
          action: actionName,
          fields: actionConfig.validation
        });
      }
    }
  }
  
  return rules;
}

module.exports = { loadConfig, buildServiceRoutes, getValidationRules };
