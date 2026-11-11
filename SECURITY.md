# Security policy

NovaHelm's apps run with deep access to your machine: NovaGear talks to USB and HID devices, opens a local API on
`127.0.0.1` and can type keys and open apps for you. We take reports seriously.

## Reporting a vulnerability

Please **don't** open a public issue, discussion or pull request for a security problem.

Report it privately through GitHub: on this repository's **Security** tab, choose **Report a vulnerability**. Include:

- what is affected (folder, version or commit, operating system);
- how to reproduce it, as small as you can make it;
- what an attacker could do with it.

We aim to answer within 3 working days and to agree a fix and a disclosure date with you. We credit reporters in the
release notes unless you'd rather we didn't.

## Supported versions

Fixes go into `main` and the next release. Older releases don't get backported fixes yet.

## In scope

- The NovaGear agent's local API and panel: token handling, client tokens, the approvals flow (only the deck may
  approve), and anything a web page or another local user could reach.
- Actions that open URLs, launch apps or type keys.
- Secrets, tokens or serial numbers that end up in logs, bug-report copies or crash output.
- The firmware check: NovaGear reads Elgato's public update feed to tell you when a Key Light has newer firmware. It
  doesn't install firmware itself yet.

## Out of scope

- Problems that need an attacker who already runs code as your user, unless NovaGear makes them worse.
- Vulnerabilities in the vendors' own firmware or apps. Report those to the vendor.
