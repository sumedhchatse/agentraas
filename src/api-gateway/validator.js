function validatePayload(service, action, payload, rules) {
  const rule = rules.find(r => r.service === service && r.action === action);
  if (!rule) return null; // No validation rules for this action
  
  for (const [fieldName, fieldRules] of Object.entries(rule.fields)) {
    const value = payload[fieldName];
    
    // Required check
    if (fieldRules.required && (value === undefined || value === null || value === '')) {
      return `${fieldName} is required`;
    }
    
    // Skip further checks if value is missing and not required
    if (value === undefined || value === null) continue;
    
    // Type check
    if (fieldRules.type) {
      const actualType = Array.isArray(value) ? 'array' : typeof value;
      if (actualType !== fieldRules.type) {
        return `${fieldName} must be ${fieldRules.type}, got ${actualType}`;
      }
    }
    
    // Number checks
    if (fieldRules.type === 'number' || typeof value === 'number') {
      if (fieldRules.min !== undefined && value < fieldRules.min) {
        return `${fieldName} must be at least ${fieldRules.min}`;
      }
      if (fieldRules.max !== undefined && value > fieldRules.max) {
        return `${fieldName} must be at most ${fieldRules.max}`;
      }
    }
    
    // String checks
    if (typeof value === 'string') {
      if (fieldRules.maxLength !== undefined && value.length > fieldRules.maxLength) {
        return `${fieldName} must be at most ${fieldRules.maxLength} characters`;
      }
      if (fieldRules.minLength !== undefined && value.length < fieldRules.minLength) {
        return `${fieldName} must be at least ${fieldRules.minLength} characters`;
      }
      if (fieldRules.format === 'email' && !/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(value)) {
        return `${fieldName} must be a valid email`;
      }
      if (fieldRules.enum && !fieldRules.enum.includes(value)) {
        return `${fieldName} must be one of: ${fieldRules.enum.join(', ')}`;
      }
    }
    
    // Array checks
    if (Array.isArray(value)) {
      if (fieldRules.minLength !== undefined && value.length < fieldRules.minLength) {
        return `${fieldName} must have at least ${fieldRules.minLength} items`;
      }
    }
  }
  
  return null;
}

module.exports = { validatePayload };
