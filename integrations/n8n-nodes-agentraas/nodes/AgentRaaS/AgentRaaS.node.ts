import type {
	IExecuteFunctions,
	INodeExecutionData,
	INodeType,
	INodeTypeDescription,
	JsonObject,
} from 'n8n-workflow';
import { NodeConnectionTypes, NodeOperationError } from 'n8n-workflow';

export class AgentRaaS implements INodeType {
	description: INodeTypeDescription = {
		displayName: 'AgentRaaS',
		name: 'agentRaaS',
		icon: 'file:agentraas.svg',
		group: ['transform'],
		version: 1,
		subtitle: '={{$parameter["service"]}}.{{$parameter["action"]}}',
		description:
			'Exactly-once execution for agent actions — wraps any API call with AgentRaaS\'s atomic dedup guarantee, so a workflow retry never double-charges or double-creates anything.',
		defaults: {
			name: 'AgentRaaS',
		},
		inputs: [NodeConnectionTypes.Main],
		outputs: [NodeConnectionTypes.Main],
		credentials: [
			{
				name: 'agentRaaSApi',
				required: true,
			},
		],
		properties: [
			{
				displayName: 'Service',
				name: 'service',
				type: 'string',
				default: '',
				required: true,
				placeholder: 'stripe',
				description:
					'A curated service from your AgentRaaS dashboard\'s Services list, or "custom" for a registered Custom Action.',
			},
			{
				displayName: 'Action',
				name: 'action',
				type: 'string',
				default: '',
				required: true,
				placeholder: 'charge.create',
				description:
					'Dotted action name for a curated service (see that service\'s docs), or your Custom Action\'s registered name when Service is "custom".',
			},
			{
				displayName: 'Payload',
				name: 'payload',
				type: 'json',
				default: '{}',
				description: 'Request body forwarded to the upstream API.',
			},
		],
	};

	async execute(this: IExecuteFunctions): Promise<INodeExecutionData[][]> {
		const items = this.getInputData();
		const returnData: INodeExecutionData[] = [];
		const credentials = await this.getCredentials('agentRaaSApi');
		const baseUrl = ((credentials.baseUrl as string) || 'http://localhost:13000').replace(/\/$/, '');
		const orgId = credentials.orgId as string;
		const agentId = credentials.agentId as string;

		for (let i = 0; i < items.length; i++) {
			try {
				const service = this.getNodeParameter('service', i) as string;
				const action = this.getNodeParameter('action', i) as string;
				const payloadRaw = this.getNodeParameter('payload', i);
				const payload =
					typeof payloadRaw === 'string' ? JSON.parse(payloadRaw || '{}') : (payloadRaw as JsonObject);

				const headers: Record<string, string> = {};
				if (orgId) headers['X-AgentRaaS-Org'] = orgId;
				if (agentId) headers['X-AgentRaaS-Agent'] = agentId;

				// Authentication (X-AgentRaaS-Key) comes from the credential's own
				// `authenticate` block — see AgentRaaSApi.credentials.ts.
				const response = await this.helpers.httpRequestWithAuthentication.call(this, 'agentRaaSApi', {
					method: 'POST',
					url: `${baseUrl}/v1/sdk/${encodeURIComponent(service)}/${encodeURIComponent(action)}`,
					headers,
					body: payload,
					json: true,
				});

				returnData.push({ json: response as JsonObject, pairedItem: { item: i } });
			} catch (error) {
				if (this.continueOnFail()) {
					returnData.push({
						json: { error: (error as Error).message },
						pairedItem: { item: i },
					});
					continue;
				}
				throw new NodeOperationError(this.getNode(), error as Error, { itemIndex: i });
			}
		}

		return [returnData];
	}
}
