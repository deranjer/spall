# Player credential operations

Sandbox player credentials are bearer secrets. The server registry uses one
line per player:

```text
<32 lowercase hex characters: stable PlayerId> <64 lowercase hex characters: 32-byte token>
```

The client receives only its token in a separate file and connects with
`--join-token-file`; it never chooses or submits the PlayerId. The game-owned
progression database is keyed by that stable ID, so rotating a token does not
reset inventory.

## Protect the files

Keep the server registry and each client token file outside the repository and
under access-controlled directories. On Windows, create the directory and
grant access only to the service identity, SYSTEM, and administrators. Replace
`NT SERVICE\SpallServer` with the actual service identity:

```powershell
$directory = 'C:\ProgramData\Spall\auth'
New-Item -ItemType Directory -Force -Path $directory | Out-Null
icacls $directory /inheritance:r /grant:r `
  'NT SERVICE\SpallServer:(OI)(CI)F' `
  'SYSTEM:(OI)(CI)F' `
  'BUILTIN\Administrators:(OI)(CI)F'
```

Protect the client token directory with the same pattern, granting access only
to that client's Windows account, SYSTEM, and administrators. Do not put token
contents in command arguments, terminal output, tickets, chat, source control,
crash reports, or application logs. `PlayerCredential` and `JoinToken` debug
formatting redacts token bytes; never add a token logging field.

## Provision

Generate a random nonzero PlayerId and a 32-byte token from the operating
system CSPRNG. Write the pair to the protected server registry and the token
alone to that player's protected client file. Write temporary files in the
same protected directory, flush them, then atomically rename them into place.
Do not reuse an ID from another account. Start `sandbox-server` with
`--player-credentials-file <registry>` and `--progression-db <database>` (or
allow the database to default under the world directory). Provide the client
token file and pinned server fingerprint over a protected channel.

## Rotate

Generate a new random token and replace only that player's token in both the
server registry and the player's client file. Preserve the existing PlayerId
exactly. Use a same-directory atomic replacement; do not temporarily truncate
the live registry. The server polls the registry every 500 ms. A valid change
is applied as one registry replacement, and active sessions are closed so the
next connection must present a currently registered token. Confirm the player
can reconnect and that their inventory revision and contents remain present.

## Revoke

Remove the player's line from the server registry and atomically replace the
file. Within the next 500 ms poll the old token is rejected, and active sessions
are closed. Verify a reconnect attempt using the old client token is rejected.
Leave the player's progression rows keyed by PlayerId intact unless a separate
data-retention decision requires deletion. To restore access later, provision
a new token for the same PlayerId.

An unreadable or malformed registry fails closed: all credentials are revoked
and active sessions are closed. Fix the protected file and atomically replace
it; a valid registry is picked up on the next poll. The service reports only
registry counts and safe parse errors, never token contents.
