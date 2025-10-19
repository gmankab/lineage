# building lineage os on github actions

### how to use

- fork repo
- go to https://github.com/settings/personal-access-tokens
- generate token with access to secrets
- use following command to add your token to to your repo secrets
- `gh secret set gh_secrets_token`
- run `generate_keys.yml` workflow to generate yggdrasil keys
- `gh workflow run generate_keys.yml`
- after keys are generated, you can run `build.yml` workflow
- `gh workflow run generate_keys.yml`

### how it works

- lineage os is known to require 300gb of storage for build
- github actions workflow provides 30gb for each runner by default
- we use easimon/maximize-build-space workflow to clean some space and get 100gb of free storage
- we use yggdrasil to combine space on 3 separate runners
- after all we are getting 300gb of space, which is enough for building lineage os

### generate_keys.yml explanation

- we generating yggdrasil private key
- getting ipv6 address from this pravate key
- writing ipv6 address to repo variables, and private key to repo secrets
- repat this 3 times for 3 jobs, each job runs on a separate runner: `nas_out`, `nas_repo`, `builder`

### build.yml explanation

- `nas_out` and `nas_repo`: setting up nas, only allow to connect from `builder` ip address
- `builder`: mounting directories from `nas_ous` and `nas_repo` runners, sync lineage os sources, and starting build lineage os
