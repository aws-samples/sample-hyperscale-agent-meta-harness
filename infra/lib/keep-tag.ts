// The keep tag of a test account. The account's cleanup service deletes a resource that lacks the tag `auto-delete` set to
// `no` (it also accepts `never`, `false`, a date or "N days") about 7 days after it first sees the resource, and warns on day 4; a tagged resource is also
// skipped by the end-of-shift stop. The tags `project`, `stage`, `owner` and `managed-by` exempt nothing: DynamoDB tables that carry all four are still
// deleted after 7 days.
//
// A leaf module with no imports: the stage app (lib/config.ts picks the entries that get it) and the two development builder apps
// share the key and the value.
export const KEEP_TAG_KEY = "auto-delete";
export const KEEP_TAG_VALUE = "no";
export const KEEP_TAG: Readonly<Record<string, string>> = { [KEEP_TAG_KEY]: KEEP_TAG_VALUE };
