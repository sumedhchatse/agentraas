// Validates a payload against one resolved rule's `fields` map. Shared by
// both the static config-driven path (validatePayload, below) and the
// dashboard-managed custom validation rules path (see
// getEffectiveValidationRule in server.js), so a rule authored in the
// Validation Rules builder is checked with the exact same logic as a
// curated service's built-in rules.
function validateFields(payload, fields) {
  for (const [fieldName, fieldRules] of Object.entries(fields)) {
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
      // E.164 phone format (WhatsApp/Twilio's expected shape): a leading
      // "+", 8-15 digits total, no leading zero after the "+". A specific
      // named format (like "email" above) rather than a generic user-
      // supplied regex — arbitrary org-authored patterns run against
      // caller-controlled payload data is a real ReDoS risk; a fixed,
      // reviewed pattern isn't.
      if (fieldRules.format === 'e164' && !/^\+[1-9]\d{7,14}$/.test(value)) {
        return `${fieldName} must be a valid E.164 phone number (e.g. +14155552671)`;
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

// Static, config-driven path — used for curated services' built-in rules
// (config/services.json → VALIDATION_RULES). Kept as its own entry point
// since it's the one call shape that predates per-org custom rules.
function validatePayload(service, action, payload, rules) {
  const rule = rules.find((r) => r.service === service && r.action === action);
  if (!rule) return null; // No validation rules for this action
  return validateFields(payload, rule.fields);
}

const VALID_FIELD_TYPES = ['string', 'number', 'boolean', 'array', 'object'];

// Structural sanity check for a rule definition submitted from the
// dashboard's Validation Rules builder — rejects nonsense (bad types,
// min > max, empty field names) before it's ever stored, so a malformed
// rule can't silently block every request for that service.action.
function isValidRuleDefinition(fields) {
  if (!fields || typeof fields !== 'object' || Array.isArray(fields) || Object.keys(fields).length === 0) {
    return 'At least one field rule is required.';
  }
  for (const [fieldName, fieldRules] of Object.entries(fields)) {
    if (!fieldName || fieldName.length > 100) {
      return `"${fieldName}" is not a valid field name.`;
    }
    if (!fieldRules || typeof fieldRules !== 'object' || Array.isArray(fieldRules)) {
      return `Field "${fieldName}" must have a rules object.`;
    }
    if (fieldRules.type !== undefined && !VALID_FIELD_TYPES.includes(fieldRules.type)) {
      return `Field "${fieldName}": type must be one of ${VALID_FIELD_TYPES.join(', ')}.`;
    }
    if (fieldRules.min !== undefined && typeof fieldRules.min !== 'number') {
      return `Field "${fieldName}": min must be a number.`;
    }
    if (fieldRules.max !== undefined && typeof fieldRules.max !== 'number') {
      return `Field "${fieldName}": max must be a number.`;
    }
    if (fieldRules.min !== undefined && fieldRules.max !== undefined && fieldRules.min > fieldRules.max) {
      return `Field "${fieldName}": min cannot be greater than max.`;
    }
    if (fieldRules.minLength !== undefined && (typeof fieldRules.minLength !== 'number' || fieldRules.minLength < 0)) {
      return `Field "${fieldName}": minLength must be a non-negative number.`;
    }
    if (fieldRules.maxLength !== undefined && (typeof fieldRules.maxLength !== 'number' || fieldRules.maxLength < 0)) {
      return `Field "${fieldName}": maxLength must be a non-negative number.`;
    }
    if (
      fieldRules.minLength !== undefined &&
      fieldRules.maxLength !== undefined &&
      fieldRules.minLength > fieldRules.maxLength
    ) {
      return `Field "${fieldName}": minLength cannot be greater than maxLength.`;
    }
    if (fieldRules.format !== undefined && !['email', 'e164'].includes(fieldRules.format)) {
      return `Field "${fieldName}": format only supports "email" or "e164" currently.`;
    }
    if (fieldRules.enum !== undefined) {
      if (!Array.isArray(fieldRules.enum) || fieldRules.enum.length === 0 || !fieldRules.enum.every((v) => typeof v === 'string')) {
        return `Field "${fieldName}": enum must be a non-empty array of strings.`;
      }
    }
  }
  return null;
}

// Structural sanity check for a per-field dedup rule (see
// getEffectiveDedupRule/custom_dedup_rules in server.js) — just a list of
// field names to build the dedup key from, so the check is much thinner
// than isValidRuleDefinition above (no per-field type/format rules here,
// just names).
const MAX_DEDUP_FIELDS = 10;
function isValidDedupRuleDefinition(fields) {
  if (!Array.isArray(fields) || fields.length === 0) {
    return 'At least one field name is required.';
  }
  if (fields.length > MAX_DEDUP_FIELDS) {
    return `At most ${MAX_DEDUP_FIELDS} fields can be used for a dedup key.`;
  }
  const seen = new Set();
  for (const f of fields) {
    if (typeof f !== 'string' || f.length === 0 || f.length > 100) {
      return `"${f}" is not a valid field name.`;
    }
    if (seen.has(f)) return `Field "${f}" is listed more than once.`;
    seen.add(f);
  }
  return null;
}

module.exports = { validatePayload, validateFields, isValidRuleDefinition, isValidDedupRuleDefinition };
