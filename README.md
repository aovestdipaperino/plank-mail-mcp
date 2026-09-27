# plank-mail-mcp

> **Status:** HAL, the plank profile this was written for, currently gets its
> mail from Softeria's
> [`ms-365-mcp-server`](https://github.com/Softeria/ms-365-mcp-server) limited
> to mail tools, because this server has no published app registration yet.
> This one remains the stricter option: it enforces in code that moves go only
> to `HAL-processed` and that updates touch only the read flag, which Softeria's
> server leaves to the agent's prompt. It works today with your own app
> registration in `client_id` (see the last section).

An [MCP](https://modelcontextprotocol.io) server over one Outlook mailbox, built
for [plank](https://github.com/aovestdipaperino/plank)'s HAL profile. It lets an
agent read mail and tidy it up, but not send or delete anything.

## What it can and cannot do

| Allowed | How |
|---|---|
| List folders, list and search messages, read a message | read-only Graph calls |
| Mark a message read or unread | `PATCH` of `isRead` only |
| Save a draft, new or as a reply | created in Drafts, never sent |
| Move a message to `HAL-processed` | the only move destination; the folder is created on first use |

It **cannot send mail**: the sign-in never asks for Microsoft's `Mail.Send`
permission, so no token it holds is able to send, whatever the code does. It
**cannot delete mail**: the permission it does hold (`Mail.ReadWrite`) would
allow deleting, so the server simply has no delete operation, and because the
only possible move is into `HAL-processed`, a move cannot stand in for a delete
(nothing can be moved to Deleted Items).

These are all the Microsoft Graph requests the server makes:

| Tool | Request |
|---|---|
| `list_folders` | `GET /me/mailFolders` |
| `list_messages` | `GET /me/mailFolders/{folder}/messages` |
| `search_messages` | `GET /me/messages?$search=...` |
| `read_message` | `GET /me/messages/{id}`, and `GET .../attachments` for names only |
| `mark_read` | `PATCH /me/messages/{id}` with `isRead` |
| `create_draft` | `POST /me/messages`, or `POST .../createReply` then `PATCH` of recipients |
| `move_to_processed` | `GET`/`POST /me/mailFolders` (find or create `HAL-processed`), `POST /me/messages/{id}/move` |

## Install

    cargo install --git https://github.com/aovestdipaperino/plank-mail-mcp

macOS only for now: the sign-in is kept in the Keychain.

## Sign in

    plank-mail-mcp login

Your browser opens at Microsoft's sign-in page. Sign in and approve, and
Microsoft sends the browser back to a one-shot listener this command opens on
`127.0.0.1`; the tab then says you can close it. The exchange uses PKCE and a
`state` check, so a redirect meant for another program cannot complete it. The
refresh token is stored in the macOS Keychain under the service
`plank-mail-mcp`; nothing is written to disk in plain text.

    plank-mail-mcp status    # who is signed in
    plank-mail-mcp logout    # forget the sign-in

Personal Outlook.com accounts and work or school Microsoft 365 accounts both
work. Some organisations require an administrator to approve third-party apps
before `Mail.ReadWrite` can be granted; if the sign-in page says so, that
approval is the only way through.

## Configure

`~/.plank/hal/mail.toml`, where HAL's `.mcp.json` points. Every key is
optional, and a missing file means the defaults:

```toml
provider = "outlook"          # the only provider so far
account = "me@outlook.com"    # a sign-in hint, and the Keychain entry's name
client_id = "..."             # use your own app registration (see below)
```

Use another file with `--config PATH`.

## With plank

HAL starts it for you:

    plank --profile aovestdipaperino/plank-profiles:HAL

Sign in once with `plank-mail-mcp login` in a terminal first. Until then the
server still starts, and every tool answers that the mailbox is not signed in.

## Using your own app registration

The server signs in as a Microsoft Entra application. To use your own instead
of the built-in one, register it in the Azure portal:

1. Microsoft Entra ID, App registrations, New registration.
2. Supported account types: accounts in any organizational directory and
   personal Microsoft accounts.
3. Redirect URI: platform "Public client/native (mobile & desktop)", value
   `http://localhost`. Microsoft accepts any port on localhost for this
   platform, which is what lets `login` pick a free one each time.
4. API permissions: add Microsoft Graph delegated `Mail.ReadWrite`,
   `User.Read` and `offline_access`. Do not add `Mail.Send`.

Put its Application (client) ID in `client_id`. It is not a secret.

## Licence

MIT
