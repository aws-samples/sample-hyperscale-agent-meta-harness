// CDK entry point: `cdk synth -c stage=live|drill` (run through scripts/node.sh). The CLI passes
// context in CDK_CONTEXT_JSON and the output directory in CDK_OUTDIR, which App reads itself.
import { buildApp } from "../lib/app.ts";
import { entryOptions } from "../lib/entry.ts";

buildApp(entryOptions(process.env)).synth();
