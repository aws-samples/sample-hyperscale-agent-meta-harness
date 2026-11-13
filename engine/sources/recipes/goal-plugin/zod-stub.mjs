// create.ts of the Cline SDK tests `inputSchema instanceof z.ZodType` to accept a zod schema. The goal plugin passes plain JSON-schema objects,
// so the test is false for every call and no zod code is needed; an empty class keeps the import valid.
export class ZodType {}
export const z = { ZodType };
