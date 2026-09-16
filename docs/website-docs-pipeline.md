# Website docs pipeline

`cli.zirv.io` renders this repository's `README.md`. Its own site source
lives in a separate repository, [`Glubiz/zirv-generic-frontend`](https://github.com/Glubiz/zirv-generic-frontend)
(see README's own "Development Workflows" section) -- not in this one.

This repository has no committed pipeline step that publishes to
`cli.zirv.io`: neither `.github/workflows/ci.yaml` nor `cd.yaml` references
the site, an S3 bucket, or a deploy step for it. Whatever build/publish
process `zirv-generic-frontend` uses to turn this README into the live site
is out of scope for this repository and not verifiable from here -- see that
repository directly for its own deployment state.
