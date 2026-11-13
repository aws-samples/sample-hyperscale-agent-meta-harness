// create.ts imports zodToJsonSchema for the zod branch the goal plugin never takes. If a plugin ever passes a zod schema this stops it loudly.
export function zodToJsonSchema(_schema: unknown): never {
	throw new Error("zod schemas are not bundled with this plugin");
}
