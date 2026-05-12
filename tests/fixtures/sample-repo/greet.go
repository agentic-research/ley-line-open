package main

// Greet builds a greeting for the given name using the shared
// formatGreeting helper. Cross-file reference target for the
// cluster:smoke test — main.go calls this, util.go's
// formatGreeting is called from here.
func Greet(name string) string {
	return formatGreeting("Hello, %s!", name)
}
