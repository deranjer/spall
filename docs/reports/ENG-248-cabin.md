# ENG-248: modular cabin

Delivered `fixtures/cabin`: an 8 x 10 m SPVOX timber shell with porch,
windows, stone hearth/chimney and four interior starts. Bed platform, table,
bench and lamp are separate SPVOX assets with local origins and independent
editor scene placements. They remain normal destructible terrain after the
existing scene importer flattens the project; no pickup system is implied.

Changed: `tools/spall_editor/examples/cabin.rs`, `examples/sandbox/tests/cabin.rs`,
`fixtures/cabin/` (project, scene, eight assets including the ENG-249 door,
and README), root README. See ENG-249 report for integrated interaction checks.

Checks:
- `cargo run -p spall_editor --example cabin`: passed, saved and reopened project.
- `cargo test -p sandbox --test cabin`: 2 passed. Real capsule stands inside,
  walks through doorway onto porch and returns; all geometry is supported.
- `cargo fmt --all`: passed.
- `cargo xtask check`: failed in unrelated `animal_death` test waiting 60 seconds
  for its server fingerprint file. The full workspace suite is not green.
- Bounded graphical capture subsequently passed with host GPU access; exterior
  and spawn view inspected. This is not a hands-on keyboard playtest.

No public API/config additions, version bumps or proportionality exceptions.
Next assigned work: ENG-249 proximity E door interaction.
