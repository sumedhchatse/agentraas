"""
AgentRaaS Python SDK
Drop-in exactly-once execution for your agents.
"""

import requests
import json

DEFAULT_BASE_URL = "http://localhost:13000/v1/sdk"

class ProtectedClient:
    """
    Wraps any API service with AgentRaaS exactly-once protection.
    """
    
    def __init__(self, service, api_key, agentraas_key, base_url=None):
        """
        Initialize protected client.
        
        Args:
            service: Service name (stripe, calendly, hubspot, mockpay)
            api_key: Your real API key for the service
            agentraas_key: Your AgentRaaS API key
            base_url: Optional custom AgentRaaS proxy URL
        """
        self.service = service
        self.api_key = api_key
        self.agentraas_key = agentraas_key
        self.base_url = base_url or DEFAULT_BASE_URL
        
    def request(self, action, payload=None):
        """
        Make a protected request.
        
        Args:
            action: Action name (e.g., 'charge.create', 'payment.create')
            payload: Request body dict
            
        Returns:
            Response dict from upstream service (or cached result)
        """
        url = f"{self.base_url}/{self.service}/{action}"
        headers = {
            "Content-Type": "application/json",
            "X-AgentRaaS-Key": self.agentraas_key
        }
        
        response = requests.post(url, headers=headers, json=payload or {}, timeout=30)
        
        if response.status_code >= 400:
            error_data = response.json()
            raise AgentRaaSError(
                error_data.get('error', 'Unknown error'),
                status_code=response.status_code,
                req_id=error_data.get('reqId')
            )
            
        return response.json()
    
    def __getattr__(self, action):
        """
        Allow dot notation: client.charge.create(payload)
        """
        return lambda payload=None: self.request(action, payload)


class AgentRaaSError(Exception):
    """Custom exception for AgentRaaS errors."""
    
    def __init__(self, message, status_code=None, req_id=None):
        super().__init__(message)
        self.status_code = status_code
        self.req_id = req_id


def protect(service, api_key, agentraas_key, base_url=None):
    """
    Convenience function to create a protected client.
    
    Usage:
        import agentraas
        stripe = agentraas.protect("stripe", api_key="sk_...", agentraas_key="ak_...")
        result = stripe.request("charge.create", {"amount": 5000, "currency": "usd"})
    """
    return ProtectedClient(service, api_key, agentraas_key, base_url)


__all__ = ['protect', 'ProtectedClient', 'AgentRaaSError']
