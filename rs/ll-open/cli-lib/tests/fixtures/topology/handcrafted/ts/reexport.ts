// Exercises the `export ... from` parser branch added for finding 11.
// Re-exports `x` from ./util — should produce a TS edge reexport.ts → util.ts.
export { x } from './util';
