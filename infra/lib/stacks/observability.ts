// Observability stack of stage `direct`:
// the Logs Insights saved query of the Bedrock invocation log. The stage builds no CloudTrail trail (CloudTrail allows five trails per
// Region and counts the shadow of every multi-Region trail, so an account that holds five refuses a sixth), no alarm and no dashboard.
//
// The Bedrock invocation log group /hamh/<stage>/bedrock-invocations and its own KMS key live in
// the data stack. This stack reads the group by name (the saved query) and never creates, configures or writes it.
// Invocation logging itself is enabled only by scripts/deploy.sh and only when unset.
import { Stack, type StackProps } from "aws-cdk-lib";
import * as logs from "aws-cdk-lib/aws-logs";
import type { Construct } from "constructs";
import { names, type StageConfig } from "../config.ts";

export interface ObservabilityStackProps extends StackProps {
  cfg: StageConfig;
}

export class ObservabilityStack extends Stack {
  constructor(scope: Construct, id: string, props: ObservabilityStackProps) {
    super(scope, id, props);
    const { cfg } = props;
    const p = cfg.prefix;
    const invocationLogs = names.bedrockInvocationLogGroup(cfg);

    // The reconciliation's saved query: per role session <run>.<agent> and model, the token counts
    // ham-cost-reconcile prices.
    new logs.CfnQueryDefinition(this, "RunTokensQuery", {
      name: `${p}/run-tokens`, logGroupNames: [invocationLogs], queryString: RUN_TOKENS_QUERY,
    });
  }
}

/** Tokens per role session and model from the invocation log (`ham-cost-reconcile` prices them). */
export const RUN_TOKENS_QUERY = [
  "filter ispresent(identity.arn)",
  "| parse identity.arn /assumed-role\\/[^\\/]+\\/(?<session>.+)$/",
  "| stats count(*) as calls, sum(input.inputTokenCount) as input, sum(input.cacheReadInputTokenCount) as cacheRead,"
    + " sum(input.cacheWriteInputTokenCount) as cacheWrite, sum(output.outputTokenCount) as output by session, modelId",
  "| sort session asc",
].join(" ");
