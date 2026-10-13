# Release-contract verification (BLO-103)

`verify-release-contract.yml` is a **throwaway** workflow that proves the one release
contract that cannot be checked from the outside: that the public half of the repo secret
`SUN_RELEASE_SIGNING_KEY` equals the `SUN_RELEASE_KEY` literal baked into
`scripts/install.sh`. It never echoes the secret — only a MATCH/MISMATCH verdict and a
public-key fingerprint.

It is parked here, outside `.github/workflows/`, because the agent token that wrote this
branch lacks the OAuth `workflow` scope. To run it:

    git switch diablo/verify-release-contract
    git mv .github/verify-fixtures/verify-release-contract.yml .github/workflows/
    git commit -m "ci: verify release signing contract (BLO-103)"
    git push                      # the push itself triggers the run
    gh run watch                  # read the RESULT: lines

Then delete the workflow, this directory, and the branch.

`sha256.sum` is the byte-identical unified-checksum artifact from run 38265664882
(`sha256sum` = 0bd5a3710369119c791317c471417330b07449c9a8ebe893177178946b80c5d6).
The workflow signs it with the real key and verifies it with `install.sh`'s exact
`ssh-keygen -Y verify` invocation, so a PASS is an end-to-end proof, not a string diff.

**Cheaper alternative:** whoever created the secret on 2026-10-12 still has the private
key file. `ssh-keygen -y -f <keyfile>` prints the public half locally in two seconds —
diff that against the literal in `scripts/install.sh:28` and skip CI entirely.
