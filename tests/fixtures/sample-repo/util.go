package main

import "fmt"

// formatGreeting applies a template string to a name. Called by Greet
// (greet.go). Used to validate LLO's find_callers / find_callees ops
// — formatGreeting is the callee of Greet; Greet is the caller of
// formatGreeting.
func formatGreeting(template, name string) string {
	return fmt.Sprintf(template, name)
}
