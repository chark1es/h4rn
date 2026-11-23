# Guest links

A guest link lets one teammate follow one thread in their browser. They need no
account and no h4rn install. They see the thread as it runs and can write a
message, but every message waits for you: it reaches the agent only after you
choose **Send**.

Guest links are a Labs feature. Turn on **Settings → Labs → Guest links** for
the machine that runs the thread. While it is off, every guest link on that
machine stops working; links that have not expired work again when you turn it
back on. The setting does not stop the 24-hour clock: a link whose time passes
while it is off is expired when it comes back.

## Share a thread

1. Right-click the thread in the sidebar, use the thread's ⋯ menu, or open the
   command palette and choose **Share with guest…**.
2. Choose **Create link** and copy it. Send it to one person.
3. When they write a message, it appears above your composer as a **Request**,
   and the sidebar row says **Request**. Choose **Send** to run it as a normal
   turn with the thread's current permission mode, or **Dismiss** to delete it
   and its files.

Choose **Stop sharing** to end the link at once. Creating a new link, settling
(archiving) the thread, or the 24 hours passing also ends it. Turning the Labs
setting off stops every link while it is off; unexpired links work again when it
is back on, with their waiting files intact.

The link works through your Connect relay address when the machine is linked to
Connect. Without Connect it names the daemon's own network-listener address, so
the guest needs network access to that address (the same LAN or tailnet). If
Network access is off too, the link names loopback and only works on this
machine; the share dialog says so. Only send a link to someone who can reach the
address it names.

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

The first browser that opens the link claims it with an HttpOnly cookie scoped
to that one link. Opening the same link in another browser shows "This link was
opened in another browser" instead of the thread. Clearing that browser's
cookies loses the link; ask for a new one. Two different links can be open in
the same browser at once, because each has its own cookie.

The daemon stores only a hash of the link token and of the browser cookie, never
the raw values. The price is that after a daemon restart the link keeps working
for the guest, but the owner cannot copy the URL again: create a new link to get
a fresh one, which ends the old link.

## Guest attachments

Guests can attach files with the paperclip, by pasting, or by dropping them on
the message box. One message holds up to 4 files and 10 MB in total. Images must
be PNG, JPEG, GIF or WebP. While messages wait for you, their files can total
20 MB; the guest can attach more once you send or dismiss some.

A waiting message's files live in the daemon's data directory under
`guest/<request-id>/`. Dismissing the message, ending the link, creating a new
link, settling the thread, or the 24-hour expiry deletes them. Turning the Labs
setting off only disables the link, so its waiting files stay until the link is
used again or ended. A message you send keeps its files like any other message
in the thread.

The guest's text and files run with the thread's current permission mode once
you send them. Check them first, as you would anything pasted from a chat.

## What the daemon enforces

The guest page is a small HTTP surface, not the device WebSocket protocol. Every
request carries the link token; the daemon hashes it, compares in constant time,
checks the 24-hour expiry, checks the browser cookie and then checks that the
thread named by the link still has a live link. The client never sends a thread
id, so there is nothing to tamper with.

A guest request can only read that thread's events, submit a waiting message,
and fetch an attachment that belongs to a message in that thread. Everything
else — another thread, a diff, a file, a terminal, an approval, a model change —
has no route. The narrow `guest` scope exists in the protocol for completeness;
a device pairing never grants it, because a guest has no device identity.
