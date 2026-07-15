// IR → MCP `tools/list` array (ley-line-open-beb8bb).
//
// The `JsonSchema` emitter produces a `$defs` bag of type schemas — one
// entry per struct. That is the wrong shape for an MCP tool registry: a
// registry is an ARRAY of `{name, description, inputSchema}` objects,
// one per operation. This emitter produces exactly that array so its
// output is "pluckable" into a consumer's `tools/list` response (rosary
// wraps it as `json!({ "tools": <array> })`).
//
//   [
//     {
//       "name": "rsry_bead_create",
//       "description": "<tool-level $Doc on the $Op struct>",
//       "inputSchema": {
//         "type": "object",
//         "properties": { "<field>": { "type": …, "description": …, "default": … }, … },
//         "required": ["<non-optional fields>"]
//       }
//     },
//     …
//   ]
//
// Which structs become tools: every struct carrying a `$Op` annotation
// (`_traits.capnp`), in IR declaration order (deterministic — no
// map-keyed serialization anywhere). The tool `name` is `OpInfo.name`
// verbatim (an explicit MCP name, not a fragile CamelCase→snake
// heuristic). The `inputSchema` is the object schema of the struct named
// by `OpInfo.input`, or the annotated struct itself when `input` is
// empty.
//
// Divergence from the `$defs` emitter, by design: the `inputSchema`
// object carries NO `additionalProperties: false`. MCP tool inputSchemas
// are open by convention (the live rosary registry omits it); adding it
// would make a generated schema drift from every hand-written tool.
// Property/type/description/default rendering is shared verbatim with
// `json_schema` (`render_property`), so the two emitters can't drift on
// field-shape.

use std::fmt::Write as _;

use super::json_schema::{escape_json_string, render_property};
use crate::error::{Result, SchemaBridgeError};
use crate::ir::{OpInfo, Schema, Struct};

pub fn emit(schema: &Schema) -> Result<String> {
    let mut tools: Vec<String> = Vec::new();
    for s in &schema.structs {
        // Only `$Op`-annotated structs are tools; plain data structs are
        // skipped (they're referenced by tools' inputSchemas, not tools
        // themselves).
        if let Some(op) = &s.op {
            tools.push(render_tool(schema, s, op)?);
        }
    }

    if tools.is_empty() {
        return Ok("[]\n".to_owned());
    }
    let mut out = String::from("[\n");
    out.push_str(&tools.join(",\n"));
    out.push_str("\n]\n");
    Ok(out)
}

fn render_tool(schema: &Schema, op_struct: &Struct, op: &OpInfo) -> Result<String> {
    // The MCP tool name is `OpInfo.name`, used verbatim. An empty name
    // can't identify a tool — fail loud rather than emit a nameless entry.
    if op.name.is_empty() {
        return Err(SchemaBridgeError::unmapped(
            "$Op with empty `name` (tool-definitions needs an explicit MCP tool name)",
            format!("struct {}", op_struct.name),
        ));
    }
    // A tool must carry a description ($Doc on the $Op struct) — MCP's
    // tools/list surfaces it as load-bearing UX.
    let description = op_struct.doc.as_deref().ok_or_else(|| {
        SchemaBridgeError::SchemaShape(format!(
            "$Op struct `{}` has no `$Doc` — a tool-level description is required",
            op_struct.name
        ))
    })?;

    // Resolve the input struct: `OpInfo.input` names it, or (empty) the
    // annotated struct is itself the input shape.
    let input_struct = if op.input.is_empty() {
        op_struct
    } else {
        schema
            .find_struct(&op.input)
            .ok_or_else(|| SchemaBridgeError::UnresolvedReference {
                name: format!("$Op input struct `{}`", op.input),
                location: format!("struct {}", op_struct.name),
            })?
    };
    // An MCP inputSchema is a plain `type: object`; a union input would
    // need a top-level `oneOf`, which no rosary tool uses. Fail loud.
    if input_struct.union.is_some() {
        return Err(SchemaBridgeError::unmapped(
            "$Op input struct with a union (tool inputSchema must be a plain object)",
            format!("struct {}", input_struct.name),
        ));
    }

    let mut out = String::new();
    writeln!(out, "  {{").expect("write! to String is infallible");
    writeln!(out, r#"    "name": "{}","#, escape_json_string(&op.name))
        .expect("write! to String is infallible");
    writeln!(
        out,
        r#"    "description": "{}","#,
        escape_json_string(description)
    )
    .expect("write! to String is infallible");
    writeln!(out, r#"    "inputSchema": {{"#).expect("write! to String is infallible");
    writeln!(out, r#"      "type": "object","#).expect("write! to String is infallible");

    if input_struct.fields.is_empty() {
        writeln!(out, r#"      "properties": {{}},"#).expect("write! to String is infallible");
    } else {
        writeln!(out, r#"      "properties": {{"#).expect("write! to String is infallible");
        let props: Vec<String> = input_struct
            .fields
            .iter()
            .map(|f| {
                format!(
                    "        \"{name}\": {prop}",
                    name = escape_json_string(&f.name),
                    prop = render_property(f)
                )
            })
            .collect();
        out.push_str(&props.join(",\n"));
        out.push('\n');
        writeln!(out, "      }},").expect("write! to String is infallible");
    }

    // `required` is the non-`$Optional` field subset. NOTE: no
    // `additionalProperties` key — MCP inputSchemas are open (see header).
    let required: Vec<String> = input_struct
        .fields
        .iter()
        .filter(|f| !f.optional)
        .map(|f| format!("\"{}\"", escape_json_string(&f.name)))
        .collect();
    writeln!(out, r#"      "required": [{}]"#, required.join(", "))
        .expect("write! to String is infallible");

    writeln!(out, "    }}").expect("write! to String is infallible");
    write!(out, "  }}").expect("write! to String is infallible");
    Ok(out)
}
