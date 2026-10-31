/**
 * Pure data shaping over the OpenAPI 3.1 document at `/api/openapi.json`
 * (`crates/tower-web/src/openapi.rs`), for `/docs/api` to render.
 *
 * Deliberately loose typing (`unknown` where the shape isn't relied on): this
 * mirrors a document generated elsewhere, and forking OpenAPI's vocabulary
 * into a second, stricter TypeScript source of truth here is the same
 * temptation `types.ts` already resists for the console's own DTOs.
 *
 * `groupByTag` has no knowledge of which tags exist — an unseen tag creates
 * its own group. That is the property that makes a route added to
 * `catalog.rs` show up on `/docs/api` automatically, with nothing in this
 * file to update. See `openapi.test.ts` for the test that pins this down.
 */

interface OpenApiParameter {
  name: string;
  in: string;
  description?: string;
}

interface OpenApiOperation {
  operationId: string;
  summary?: string;
  description?: string;
  tags?: string[];
  parameters?: OpenApiParameter[];
  requestBody?: { content?: { 'application/json'?: { schema?: { $ref?: string } } } };
  responses?: Record<string, { content?: { 'application/json'?: { schema?: { $ref?: string } } } }>;
  'x-otto-factory-auth'?: string;
}

export interface OpenApiDocument {
  paths: Record<string, Record<string, OpenApiOperation>>;
  components?: {
    schemas?: Record<
      string,
      { type?: string; properties?: Record<string, SchemaField>; required?: string[] }
    >;
  };
}

interface SchemaField {
  type?: string;
  description?: string;
}

export interface ParamEntry {
  name: string;
  description: string;
}

export interface EndpointEntry {
  method: string;
  path: string;
  operationId: string;
  summary: string;
  description: string;
  auth: string;
  parameters: ParamEntry[];
  requestSchema?: string;
  responseSchema?: string;
}

export interface TagGroup {
  tag: string;
  endpoints: EndpointEntry[];
}

export interface SchemaProperty {
  name: string;
  type: string;
  description?: string;
  required: boolean;
}

/** Stable rendering order for verbs sharing a path. */
const VERB_ORDER = ['get', 'post', 'put', 'patch', 'delete'];

function verbRank(verb: string): number {
  const i = VERB_ORDER.indexOf(verb);
  return i === -1 ? VERB_ORDER.length : i;
}

function refName(schema?: { $ref?: string }): string | undefined {
  const ref = schema?.$ref;
  if (!ref) return undefined;
  const i = ref.lastIndexOf('/');
  return i === -1 ? ref : ref.slice(i + 1);
}

/**
 * Only the endpoint's success response can be its "response body" — `tower-web`
 * gives every endpoint `400`/`500` (and often `401`/`403`/`404`) entries that
 * all reference the shared `Error` schema, and iterating
 * `Object.values(operation.responses)` without filtering would visit those
 * before (or instead of) a schema-less `2xx` success response in some
 * engines, since integer-like object keys enumerate in ascending numeric
 * order. Restricting to `2xx` keys is what keeps a `DELETE` endpoint with no
 * response body (`204`, no schema) from being mislabeled as returning
 * `Error`.
 */
function firstResponseSchema(operation: OpenApiOperation): string | undefined {
  for (const [status, response] of Object.entries(operation.responses ?? {})) {
    if (!status.startsWith('2')) continue;
    const name = refName(response.content?.['application/json']?.schema);
    if (name) return name;
  }
  return undefined;
}

/**
 * Flattens every verb on every path into one `EndpointEntry` each, groups by
 * the operation's first tag (`'untagged'` if none — `document()` always sets
 * one, but the frontend must not crash if that ever changes), and sorts
 * groups alphabetically and endpoints within a group by path then verb order.
 */
export function groupByTag(doc: OpenApiDocument): TagGroup[] {
  const byTag = new Map<string, EndpointEntry[]>();

  for (const [path, operations] of Object.entries(doc.paths ?? {})) {
    for (const [method, operation] of Object.entries(operations)) {
      const tag = operation.tags?.[0] ?? 'untagged';
      const entry: EndpointEntry = {
        method,
        path,
        operationId: operation.operationId,
        summary: operation.summary ?? '',
        description: operation.description ?? '',
        auth: operation['x-otto-factory-auth'] ?? 'public',
        parameters: (operation.parameters ?? []).map((p) => ({
          name: p.name,
          description: p.description ?? ''
        })),
        requestSchema: refName(operation.requestBody?.content?.['application/json']?.schema),
        responseSchema: firstResponseSchema(operation)
      };
      const list = byTag.get(tag) ?? [];
      list.push(entry);
      byTag.set(tag, list);
    }
  }

  const groups = Array.from(byTag.entries())
    .map(([tag, endpoints]) => ({
      tag,
      endpoints: endpoints.sort(
        (a, b) => a.path.localeCompare(b.path) || verbRank(a.method) - verbRank(b.method)
      )
    }))
    .sort((a, b) => a.tag.localeCompare(b.tag));

  return groups;
}

/**
 * The top-level `properties` of `components.schemas[refName]`, one level
 * deep — enough for "a reader most needs" per the design spec, without
 * building a general JSON Schema viewer. `[]` if the schema is missing or
 * isn't an object schema.
 */
export function schemaSummary(doc: OpenApiDocument, name: string): SchemaProperty[] {
  const schema = doc.components?.schemas?.[name];
  if (!schema?.properties) return [];

  const required = new Set(schema.required ?? []);
  return Object.entries(schema.properties).map(([propName, field]) => ({
    name: propName,
    type: field.type ?? 'unknown',
    description: field.description,
    required: required.has(propName)
  }));
}
