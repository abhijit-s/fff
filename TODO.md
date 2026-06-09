# TODO

## Homebrew tap: set up GitHub remote

The tap at `/opt/homebrew/Library/Taps/abhijit-s/homebrew-fff` is local-only (no remote).
Until this is done, `brew upgrade` requires a manual `git pull origin main` in the fff repo first.

Steps:
1. `gh repo create abhijit-s/homebrew-fff --public`
2. Add remote and push from `/opt/homebrew/Library/Taps/abhijit-s/homebrew-fff`
3. Create a fine-grained PAT scoped to `homebrew-fff` with Contents write, add as `HOMEBREW_TAP_TOKEN` secret in `abhijit-s/fff`
4. Update `update-homebrew-formula` CI job to checkout and push to `abhijit-s/homebrew-fff` using the PAT
