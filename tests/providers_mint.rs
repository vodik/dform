//! What the mock mints for a computed attribute (`type_mint`): a value
//! that is not a string template reaches the world as the engine writes
//! it in JSON (`spell::value_to_json`), a quantity as its canonical text.

mod common;
use common::Scratch;

#[test]
fn a_minted_quantity_is_its_canonical_text() {
    let s = Scratch::new("mint-quantity");
    s.write(
        "schema.df",
        "\ntype_provider(app.disk, \"fakecloud\")\n\
         type_attr(app.disk, \"id\", \"string\", [\"computed\", \"id\"])\n\
         type_attr(app.disk, \"size\", \"bytes\", [\"computed\"])\n\
         type_mint(app.disk, \"size\", 512Mi)\n",
    );
    s.write("p.df", "\nresource app.disk d {}\nuse fake\n");
    s.run(&[
        "dev",
        "--provider",
        "./schema.df",
        "--world",
        "w.json",
        "apply",
        "p.df",
    ])
    .success();
    let world = std::fs::read_to_string(s.path("w.json")).unwrap();
    assert!(world.contains("\"size\": \"512Mi\""), "{world}");
}
