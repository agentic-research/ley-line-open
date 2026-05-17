// Exercises the comment-skip path added for Copilot finding 11.
//
// Only the real dynamic import below should produce a TS edge. The
// commented-out `import('./should-not-match')` and the block-comment
// `/* import('./also-should-not-match') */` must NOT match. Trailing
// inline comments after a real import are still tricky; covered by
// gate2-style basename assertions in the test.
//
// import('./should-not-match')
/* import('./also-should-not-match') */
async function load() {
  const real = await import('./real-import');
  return real;
}

export { load };
