Section: Changed

- **The Go schema workflow runs on pull requests into `integration`**
  (bead `ley-line-open-a4787a`). `leyline-schema-go.yml` triggered on `main`
  only, so a change to `clients/go/leyline-schema` reached `integration`
  without GitHub running its regeneration diff or `go test`, and the first
  GitHub Go run was the promotion PR. Its push and pull-request filters now
  name `integration` as well; the path filters are unchanged.
