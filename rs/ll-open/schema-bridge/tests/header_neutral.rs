//! The generated header names the plugin that regenerates the file, not one
//! consumer's task runner. schema-bridge depends on nothing but `capnp`, and
//! cloister, notme and LLO each run it from their own build; a header naming
//! cloister's `task cluster:zod` misdirects everyone else.

use leyline_schema_bridge::ir::Schema;
use leyline_schema_bridge::{OutputFormat, emit};

#[test]
fn the_generated_header_names_the_plugin_not_a_consumers_task() {
    for (format, plugin) in [
        (OutputFormat::Zod, "capnpc-schema-bridge-zod"),
        (OutputFormat::Go, "capnpc-schema-bridge-go"),
    ] {
        let out = emit(&Schema::new(), format, "example").unwrap();
        let header: String = out
            .lines()
            .take_while(|l| l.starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(header.contains(plugin), "{plugin}: {header}");
        assert!(
            !header.contains("cluster:"),
            "{plugin} names a consumer task: {header}"
        );
    }
}
