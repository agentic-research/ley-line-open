// Decoy file. The string `./should-not-match` appears inside a
// line comment in with-comments.ts. If the import sweep mistakes the
// commented import for a real one, it WILL resolve to this file and
// the gate6 test will catch the false positive.
export const decoy = "if-this-resolves-its-a-bug";
