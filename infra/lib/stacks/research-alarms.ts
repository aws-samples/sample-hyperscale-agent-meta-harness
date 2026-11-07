// The CloudWatch alarms on the stored Tavily key. A Tavily key from the dashboard does not expire, so nothing else says that it is old, refused
// or out of credits. research-api writes four metrics as Embedded Metric Format lines (crates/research-api/src/metrics.rs) and this construct alarms on them.
// They exist only for a stage whose research provider can call Tavily (`TAVILY_CAPABLE_PROVIDERS` in lib/config.ts) and only while `HAMH_TAVILY_ALARMS` is not `off`
// (`resolveResearchAlarms`).
//
//   alarm        metric             statistic, period      fires at          missing data
//   key-age      TavilyKeyAgeDays   Maximum, 3600 s        HAMH_TAVILY_KEY_MAX_AGE_DAYS (30)   ignore: the age is written while the route is used, a silent hour keeps the last state
//   key-refused  TavilyKeyRefused   Sum, 300 s            1                 notBreaching
//   rate-limited TavilyRateLimited  Sum, 900 s            1                 notBreaching
//   plan-limit   TavilyPlanLimit    Sum, 3600 s           1                 notBreaching
//
// One datapoint of one period decides each alarm. An alarm and its return to OK go to the SNS topic of `HAMH_RESEARCH_ALARM_TOPIC_ARN` when it is set; without
// a topic the alarms only change state. L1 constructs only; no IAM statement changes (the alarms read metrics, the Lambda writes them through its log).
import * as cloudwatch from "aws-cdk-lib/aws-cloudwatch";
import { Construct } from "constructs";
import { names, type ResearchAlarmSettings, type StageConfig, type TavilyAlarm } from "../config.ts";

/**
 * The CloudWatch namespace and dimension of the research metrics and the four metric names. crates/research-api/src/metrics.rs writes the same strings;
 * test/research-alarms.test.ts reads that file and compares, and metrics.rs has a test of its own for the Rust side. They are written here once.
 */
export const RESEARCH_METRICS_NAMESPACE = "Hamh/Research";
export const RESEARCH_METRICS_DIMENSION = "Stage";
export const TAVILY_METRIC = {
  keyRefused: "TavilyKeyRefused",
  rateLimited: "TavilyRateLimited",
  planLimit: "TavilyPlanLimit",
  keyAgeDays: "TavilyKeyAgeDays",
} as const;

export interface ResearchAlarmsProps {
  cfg: StageConfig;
  settings: ResearchAlarmSettings;
}

interface AlarmSpec {
  which: TavilyAlarm;
  metric: string;
  statistic: "Maximum" | "Sum";
  period: number;
  threshold: number;
  treatMissingData: "ignore" | "notBreaching";
  description: string;
}

export class ResearchAlarms extends Construct {
  readonly alarms: cloudwatch.CfnAlarm[];

  constructor(scope: Construct, id: string, props: ResearchAlarmsProps) {
    super(scope, id);
    const { cfg, settings } = props;
    const secret = names.researchSecret(cfg);
    const actions = settings.topicArn === undefined ? undefined : [settings.topicArn];
    const specs: AlarmSpec[] = [
      {
        which: "key-age", metric: TAVILY_METRIC.keyAgeDays, statistic: "Maximum", period: 3600, threshold: settings.maxKeyAgeDays, treatMissingData: "ignore",
        description: `The Tavily key stored in the secret ${secret} is ${settings.maxKeyAgeDays} days old or older. A Tavily key from the dashboard does not expire, so only this alarm says it is old. ` +
          "Rotate it: create a new key in the Tavily dashboard and keep the old one, store the new one with scripts/research-key-put.py --stage direct, check it with --check, " +
          "wait 10 minutes, then delete the old key in the dashboard.",
      },
      {
        which: "key-refused", metric: TAVILY_METRIC.keyRefused, statistic: "Sum", period: 300, threshold: 1, treatMissingData: "notBreaching",
        description: "Tavily refused the stored key (HTTP 401 or 403) at least once in 5 minutes, so web search answers no_key. The key was probably deleted or deactivated in the Tavily dashboard, " +
          "or the wrong text was stored. Create a key in the dashboard and store it with scripts/research-key-put.py --stage direct; search works again within 5 minutes.",
      },
      {
        which: "rate-limited", metric: TAVILY_METRIC.rateLimited, statistic: "Sum", period: 900, threshold: 1, treatMissingData: "notBreaching",
        description: "Tavily limited the rate of calls (HTTP 429) at least once in 15 minutes. A Tavily development key allows 100 requests a minute and the stage sends at most 80, " +
          "so the key is probably also used somewhere else, or it is a smaller key. Wait for it to pass, or store a production key (it needs a paid plan or pay as you go) " +
          "with scripts/research-key-put.py --stage direct.",
      },
      {
        which: "plan-limit", metric: TAVILY_METRIC.planLimit, statistic: "Sum", period: 3600, threshold: 1, treatMissingData: "notBreaching",
        description: "Tavily refused a call because a usage limit was reached (HTTP 432: the limit of the key or the plan; 433: the pay-as-you-go limit) at least once in an hour. " +
          "Web search fails until the limit is raised. Raise the monthly limit of the key, the plan or the pay-as-you-go limit in the Tavily dashboard.",
      },
    ];
    this.alarms = specs.map((spec) => new cloudwatch.CfnAlarm(this, `Alarm-${spec.which}`, {
      alarmName: names.researchAlarm(cfg, spec.which),
      alarmDescription: spec.description,
      namespace: RESEARCH_METRICS_NAMESPACE,
      metricName: spec.metric,
      dimensions: [{ name: RESEARCH_METRICS_DIMENSION, value: cfg.stage }],
      statistic: spec.statistic,
      period: spec.period,
      evaluationPeriods: 1,
      datapointsToAlarm: 1,
      threshold: spec.threshold,
      comparisonOperator: "GreaterThanOrEqualToThreshold",
      treatMissingData: spec.treatMissingData,
      ...(actions === undefined ? {} : { alarmActions: actions, okActions: actions }),
    }));
  }
}
