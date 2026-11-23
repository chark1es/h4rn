# Guest links

A guest link lets one teammate follow one thread in their browser. They need no
account and no h4rn install. They see the thread as it runs and can write a
message, but every message waits for you: it reaches the agent only after you
choose **Send**.

Guest links are a Labs feature. Turn on **Settings → Labs → Guest links** for
the machine that runs the thread. Turning the setting off **ends** every link on
that machine and deletes the messages waiting for you; turning it back on does
not revive them. Share again to make a new link.

## Share a thread

1. Right-click the thread in the sidebar, use the thread's ⋯ menu, or open the
   command palette and choose **Share with guest…**.
2. Choose **Create link** and copy it. Send it to one person.
3. When they write a message, it appears above your composer as a **Request**,
   and the sidebar row says **Request**. Choose **Send** to run it as a normal
   turn with the thread's current permission mode, or **Dismiss** to delete it
   and its files.

Choose **Stop sharing** to end the link at once. Creating a new link, settling
(archiving) the thread, turning the Labs setting off, or the 24 hours passing
also ends it. An expired link and its waiting files are swept at daemon startup
and every ten minutes after that.

The link works through your Connect relay address when the machine is linked to
Connect. Without Connect a direct link names the daemon's loopback address, so
it works only in a browser on this machine; the share dialog says so. A direct
link is never offered over a plain LAN address: the link is a bearer token, and
the daemon has no HTTPS listener for a teammate's browser. To share with someone
on another machine, link this machine to Connect first.

## What the guest sees

The guest sees the thread's recent history and everything after it, in the same
timeline you see: your messages and their files, the agent's replies, its
reasoning and plans, its tool calls with expandable output, and the images it
shows. The page keeps following the thread while it is open.

The guest does **not** see diffs, other threads, your file system, terminals or
settings. They cannot answer the agent's questions or approvals, change the
model or permission mode, fork, revert, stop a turn, or run anything. Their
composer names the thread's model but cannot change it.

**Anything the agent read or printed can show**, such as a value from a file or
a command's output. Tool output is part of the conversation. Share only threads
you are comfortable showing to that person.

## One browser

The first browser that reads the link's state claims it with a `Secure;
HttpOnly; SameSite=Strict` cookie scoped to that one link. Loading the page
itself never claims it, so a link-preview crawler that merely fetches the URL
cannot take the link; the page offers a short-lived unclaimed cookie, and the
first state read or message submission that presents it binds that browser.
Opening the same link in another browser shows "This link was opened in another
browser" instead of the thread. Clearing that browser's cookies loses the link;
ask for a new one. Two different links can be open in the same browser at once,
because each has its own cookie and path.

The daemon stores only a hash of the link token and of the browser cookie, never
the raw values. The price is that after a daemon restart the link keeps working
for the guest, but the owner cannot copy the URL again: create a new link to get
a fresh one, which ends the old link.

## Guest attachments

Guests can attach files with the paperclip, by pasting, or by dropping them on
the message box. One message holds up to 4 files and 10 MB in total, and at most
32 KB of text. Images must be PNG, JPEG, GIF or WebP, checked by the file's bytes
rather than its name. While messages wait for you, at most 8 messages and 20 MB
in total (text and files together) may be queued; the guest can send more once
you send or dismiss some.

A waiting message's files live in the daemon's data directory under
`guest/<request-id>/`. Dismissing the message, ending the link, creating a new
link, settling the thread, the 24-hour expiry, or turning the Labs setting off
deletes them. Ending access and deleting files are separate steps: the link row
is removed first, so a disk error can never leave a link live, and the periodic
sweep retries the file cleanup. A message you send keeps its files like any
other message in the thread.

The guest's text and files run with the thread's current permission mode once
you send them. Check them first, as you would anything pasted from a chat.

## What the daemon enforces

The guest page is a small HTTP surface, not the device WebSocket protocol. Every
request carries the link token; the daemon hashes it, compares in constant time,
checks the 24-hour expiry, checks the browser cookie and then checks that the
thread named by the link still has a live link. The client never sends a thread
id, so there is nothing to tamper with. A message submission checks the token and
binding from its headers before any of the body is read, requires a same-origin
JSON POST, reserves the pending quota and inserts the request in one transaction,
and deletes any files it wrote if the link ended meanwhile. Send and Dismiss each
claim a request with one conditional update, so two actions can never run the
same message.

A guest request can only read that thread's events, submit a waiting message,
and fetch an attachment that belongs to a message in that thread. Events are
projected through an allow-list, so the guest never sees the workspace path,
approvals, filesystem events, subagent threads or MCP administration. Tool calls
lose their raw arguments, and a file-editing tool loses its output too, so a
Codex file change's diff or a patch never reaches the guest. Everything else —
another thread, a diff, a file, a terminal, an approval, a model change — has no
route. The narrow `guest` scope exists in the protocol for completeness; a
device pairing never grants it, because a guest has no device identity.

Only the machine's owner, or a paired device with the admin scope, may copy a
link's URL; a view-only device sees that a link exists and when it expires, but
the URL comes back `null`. The URL is the credential, so those requests are
audited.

Through Connect, the guest page itself is served by Connect from its own fixed,
hash-checked CSP document; Connect never relays a daemon's HTML. Only JSON and
an allow-list of media types come back from the daemon, and Connect rebuilds the
headers it passes on, including the binding cookie's path, which must name the
public `/guest/<machine>/<token>` prefix. Request and response bodies travel in
bounded 512 KB frames, so a 10 MB upload never grows a shared relay socket frame;
Connect refuses an exchange over the guest limit before it reaches the socket.
Guest routes are traced by their template (`/guest/{token}/state`), never by the
raw URI, so the token does not appear in logs.
