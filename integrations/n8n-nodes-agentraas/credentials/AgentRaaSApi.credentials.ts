import type { ICredentialType, INodeProperties, IAuthenticateGeneric } from 'n8n-workflow';

export class AgentRaaSApi implements ICredentialType {
	name = 'agentRaaSApi';

	displayName = 'AgentRaaS API';

	documentationUrl = 'https://github.com/sumedhchatse/agentraas';

	properties: INodeProperties[] = [
		{
			displayName: 'Base URL',
			name: 'baseUrl',
			type: 'string',
			default: 'http://localhost:13000',
			placeholder: 'https://your-deployment.example.com',
			description:
				'Your AgentRaaS deployment — self-hosted (e.g. http://localhost:13000) or your AgentRaaS Cloud URL.',
		},
		{
			displayName: 'Agent API Key',
			name: 'agentKey',
			type: 'string',
			typeOptions: { password: true },
			default: '',
			required: true,
			description:
				'From the dashboard\'s "+ Connect Agent" panel (ar_live_...).',
		},
		{
			displayName: 'Org ID',
			name: 'orgId',
			type: 'string',
			default: '',
			description:
				'Recommended: without this, requests fall back to an unenforced shared identity — see the AgentRaaS docs.',
		},
		{
			displayName: 'Agent ID',
			name: 'agentId',
			type: 'string',
			default: '',
			description: 'Recommended alongside Org ID — same identity you connected the agent under.',
		},
	];

	// AgentRaaS auth is a custom header, not Bearer/Basic — declared here so
	// nodes using this credential via routing (or a future declarative-style
	// node) authenticate automatically instead of setting the header by hand.
	authenticate: IAuthenticateGeneric = {
		type: 'generic',
		properties: {
			headers: {
				'X-AgentRaaS-Key': '={{$credentials.agentKey}}',
			},
		},
	};
}
