# Contributing to NovaHelm

Thanks for helping. Bug reports, device records, fixes and new features are all welcome.

## Before you start

- For a bug, open an issue with what you did, what you expected and what happened. For NovaGear, the panel's Logs tab
  has "Copy for a bug report", which leaves out serial numbers and tokens.
- For a new feature or a large change, open an issue first so we can agree on the shape before you write the code.
- Security problems go through [SECURITY.md](SECURITY.md), not a public issue.

## Adding a device to NovaGear

There are two levels, and most devices only need the first:

1. **A device record.** A few lines of TOML that name a device and point it at a driver NovaGear already has (another
   Razer mouse, another Elgato light, a capture card). No Rust needed. Use the panel's **Copy device details** to get a
   draft, test it in your own `devices.toml`, and send a pull request with the device-record checklist.
2. **A new driver.** Code, for a device that speaks a protocol NovaGear doesn't know yet. Open a "Request a device"
   issue first so we can agree on the approach.

The format, an annotated example and the checker are in
[novagear/crates/drivers/devices/README.md](novagear/crates/drivers/devices/README.md).

## Licences

Each top-level folder has its own `LICENSE`. Apps that run on your machine and the shared kits (for example
`novagear/` and `novaui/`) are Apache-2.0. The cloud, once it is published here, is AGPL-3.0. A contribution to a folder
is made under that folder's licence. We won't change these licences, and there is no CLA.

## Sign your commits (DCO)

We use the [Developer Certificate of Origin](https://developercertificate.org/) (DCO) instead of a CLA. By adding a
`Signed-off-by` line to a commit, you state that you wrote the change or otherwise have the right to submit it under the
folder's open-source licence. The full text is below.

Add the line with `-s`:

```bash
git commit -s -m "novagear: add the Key Light Air record"
```

It must use your real name and an email you can be reached at, and match the commit's author:

```text
Signed-off-by: Your Name <you@example.com>
```

Forgot it? `git commit --amend -s` fixes the last commit, and `git rebase --signoff main` fixes every commit on your
branch. A check on each pull request looks for the line.

## Making a change

1. Fork the repository and create a branch from `main`.
2. Keep each pull request to one change, with tests where the code has them.
3. Run the checks for the folder you touched:
   - `novagear/`: `cargo fmt --all --check` and `cargo test --workspace --locked` in `novagear/`, and
     `swift test --package-path novagear/apple` on a Mac.
   - `novaui/`: `swift test --package-path novaui`.
4. Never commit secrets, tokens, real serial numbers or your own machine's paths. Test fixtures use made-up values.
5. Open the pull request and describe what changed and how you tested it, including the hardware you tried it on.

## Developer Certificate of Origin

```text
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```
