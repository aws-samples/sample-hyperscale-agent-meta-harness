// The AgentCore Gateway of the `agentcore` search provider. research-api signs a JSON-RPC call (SigV4, service
// bedrock-agentcore) to this gateway, which serves AWS's own web index through the Web Search Tool connector. The gateway, its service role and its one
// target exist only for a stage that selects the provider (HAMH_RESEARCH_PROVIDER=agentcore) or leaves the choice to `auto`; the tavily,
// tavily-keyless and fixture stages synthesize what they did before. It is a construct of the research route (control stack), not a tenth stack, like the
// rest of the research resources.
//
// What is here: the service role the gateway assumes, the gateway (MCP, AWS_IAM authorization, so a caller signs with its own role and needs
// bedrock-agentcore:InvokeGateway on this gateway), and the target `web-search-tool` (connector `web-search`, credential: the gateway's own role).
// Nothing here changes the VPC, its endpoints, the Network Firewall allowlist or the worker session policy: research-api runs outside the VPC and
// reaches the gateway's public host itself (invariant 3 of the harness stays as it is).
//
// The target is CloudFormation's own AWS::BedrockAgentCore::GatewayTarget, not a custom resource. Its ConnectorSource has one property, ConnectorId, so the
// connector version cannot be written here. AWS resolves a target created without a version to the connector's current default and keeps that version on
// every update ("Connector versions", AWS docs; minor versions are backward compatible). A deploy therefore records the resolved version
// (`aws bedrock-agentcore-control get-gateway-target`, the three outputs below name what to ask for). A custom resource could pin 1.2.0, but its single-call
// delete cannot wait for the target to disappear before the gateway is deleted, and an older SDK in the function would drop the field without a word.
import { CfnOutput, Stack } from "aws-cdk-lib";
import * as agentcore from "aws-cdk-lib/aws-bedrockagentcore";
import * as iam from "aws-cdk-lib/aws-iam";
import { Construct } from "constructs";
import { ACCOUNT, names, type StageConfig, WEB_SEARCH_CONNECTOR, WEB_SEARCH_TARGET } from "../config.ts";

export interface ResearchGatewayProps {
  cfg: StageConfig;
}

export class ResearchGateway extends Construct {
  /** The gateway's ARN: research-api's `InvokeGateway` grant names it. */
  readonly gatewayArn: string;
  /** The gateway's MCP endpoint (it ends `/mcp`): research-api's `AGENTCORE_GATEWAY_URL`. */
  readonly gatewayUrl: string;
  /** The L1 gateway and target, for dependencies and tests. */
  readonly gateway: agentcore.CfnGateway;
  readonly target: agentcore.CfnGatewayTarget;

  constructor(scope: Construct, id: string, props: ResearchGatewayProps) {
    super(scope, id);
    const { cfg } = props;
    const region = Stack.of(this).region;
    const gatewayName = names.researchGateway(cfg);

    // ---- the service role: AgentCore assumes it to look the gateway up and to authorize the connector's search ------------------------
    const role = new iam.CfnRole(this, "Role", {
      roleName: names.role(cfg, "research-gateway"),
      description: `${cfg.prefix} AgentCore Gateway service role: the gateway assumes it to authorize the Web Search Tool connector`,
      assumeRolePolicyDocument: {
        Version: "2012-10-17",
        Statement: [{
          Effect: "Allow", Principal: { Service: "bedrock-agentcore.amazonaws.com" }, Action: "sts:AssumeRole",
          // The form the AWS documentation gives: this account and the gateways of this Region.
          Condition: {
            StringEquals: { "aws:SourceAccount": ACCOUNT },
            ArnLike: { "aws:SourceArn": `arn:aws:bedrock-agentcore:${region}:${ACCOUNT}:gateway/*` },
          },
        }],
      },
      policies: [{
        policyName: "research-gateway",
        policyDocument: {
          Version: "2012-10-17",
          Statement: [
            // The gateway needs the role before it exists, so the role cannot name the gateway's ARN (a CloudFormation cycle). It names the gateway by
            // the id the service gives, `<name>-<10 characters>`.
            { Sid: "GetGateway", Effect: "Allow", Action: ["bedrock-agentcore:GetGateway"],
              Resource: `arn:aws:bedrock-agentcore:${region}:${ACCOUNT}:gateway/${gatewayName}-*` },
            // The connector's authorization is checked per request against the service-owned tool ARN (account `aws`). The trailing star is the form
            // the working gateway used; it also matches the documented exact ARN.
            { Sid: "InvokeWebSearch", Effect: "Allow", Action: ["bedrock-agentcore:InvokeWebSearch"],
              Resource: `arn:aws:bedrock-agentcore:${region}:aws:tool/web-search.v1*` },
          ],
        },
      }],
    });

    // ---- the gateway: MCP, signed callers only -------------------------------------------------------------------------------------------
    this.gateway = new agentcore.CfnGateway(this, "Gateway", {
      name: gatewayName,
      description: `${cfg.prefix} web search for the research route: the Web Search Tool connector behind IAM-signed MCP calls`,
      protocolType: "MCP",
      authorizerType: "AWS_IAM",
      roleArn: role.attrArn,
    });

    // ---- the target: the Web Search Tool connector, credential = the gateway's role ----------------------------------------------------
    this.target = new agentcore.CfnGatewayTarget(this, "Target", {
      gatewayIdentifier: this.gateway.attrGatewayIdentifier,
      name: WEB_SEARCH_TARGET,
      description: "AWS web search (Web Search Tool connector)",
      targetConfiguration: {
        mcp: { connector: { source: { connectorId: WEB_SEARCH_CONNECTOR }, configurations: [{ name: "WebSearch", parameterValues: {} }] } },
      },
      credentialProviderConfigurations: [{ credentialProviderType: "GATEWAY_IAM_ROLE" }],
    });

    this.gatewayArn = this.gateway.attrGatewayArn;
    this.gatewayUrl = this.gateway.attrGatewayUrl;

    // What the post-deploy check asks AgentCore for: `get-gateway-target --gateway-identifier <id> --target-id <id>` answers with the resolved version.
    // The outputs belong to the stack itself, so their logical ids are exactly these names.
    const stack = Stack.of(this);
    new CfnOutput(stack, "ResearchGatewayId", { value: this.gateway.attrGatewayIdentifier, description: "AgentCore Gateway id of the web search provider" });
    new CfnOutput(stack, "ResearchGatewayUrl", { value: this.gatewayUrl, description: "MCP endpoint research-api signs calls to (AGENTCORE_GATEWAY_URL)" });
    new CfnOutput(stack, "ResearchGatewayTargetId", { value: this.target.attrTargetId, description: "id of the web-search-tool target (read its resolved connector version)" });
  }
}
