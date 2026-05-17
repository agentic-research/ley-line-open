// Decoy file. The string `./also-should-not-match` appears inside a
// block comment in with-comments.ts. If the block-comment import
// leaks through the parser, this file WILL resolve and gate6 will
// catch the false positive.
export const decoy = "if-this-resolves-its-a-bug";
