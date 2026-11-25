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
also ends it. A settled thread cannot be shared again until it is reopened: the
menu, the command palette and the header hide **Share with guest…**, and the
daemon refuses a new link, a submission or a Send on it. An expired link and its
waiting files are swept at daemon startup and every ten minutes after that; the
sweep also ends a link left on a thread that settled while the daemon was off.

The link works through your Connect relay address when the machine is linked to
Connect. Without Connect a direct link names the first listener that can answer
it on this machine: the loopback address of the primary listener, a wildcard
bind such as `--bind 0.0.0.0:47800`, or a loopback `--extra-bind`. It uses
`127.0.0.1` for an IPv4 wildcard bind and `[::1]` for an IPv6 loopback or
wildcard bind, and a concrete loopback bind such as `127.0.0.2` names itself,
on the port that listener actually bound. An IPv4-mapped loopback bind such as
`[::ffff:127.0.0.2]` names the IPv4 address it really listens on, never
`[::1]`, where nothing would answer. Any address in `127.0.0.0/8` counts
as loopback both when the listener is chosen and when the link's URL is
checked. A direct link works only in a browser on this machine; the share
dialog says so. Any request that arrives from a non-loopback address answers
404, including a connection from the network to a wildcard socket. A listener
bound to one concrete non-loopback address serves no guest route at all, and
the LAN listener (Settings → Network access) does not either, so a direct guest
path there answers 404. If every listener is a concrete network address there
is no direct link: sharing is refused with the remedy, and an existing link
says it has no address it can open on. The dialog says to add a loopback
listener or link this machine to Connect instead of handing out a link that
cannot answer. To share with someone on another machine, link this machine to
Connect first.

## What the guest sees

The guest sees the thread's recent history and everything after it, in the same
timeline you see: your messages and their files, the agent's replies, its
reasoning and plans, its tool calls with expandable output, and the images it
shows. The page keeps following the thread while it is open.

A guest sees only its own queued messages. Replacing the link creates a new
guest, and that guest's page never replays an earlier link's waiting or
dismissed message text or file names; a message you sent appears in the
transcript like any other message.

The guest does **not** see diffs, other threads, your file system, terminals or
settings. They cannot answer the agent's questions or approvals, change the
model or permission mode, fork, revert, stop a turn, or run anything. Their
composer names the thread's model but cannot change it.

An agent error reaches the owner with its full diagnostic, which can name the
workspace or a path on this machine. The guest sees a plain "The agent hit an
error." instead, plus whether trying again is safe, so a startup failure's
absolute path never leaves the machine.

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

A page for an expired, revoked, switched-off or already-bound link opens on its
notice with no composer: the daemon renders the link's state into the page, and
the served markup starts with that notice visible and the composer hidden, so
the first paint before any script runs is already correct. Connect passes the
same state through to the page it serves from its own origin, so the guest
never sees a message box that cannot work while the first poll runs.
Connect also sends the browser's existing binding cookie with that page request,
so a bound browser reloads to its own state while a fresh browser on the same
link sees "unbound"; a page load still never claims the link. A token the
daemon no longer knows answers 410, which Connect opens on the revoked notice.
A direct guest connection from an IPv4 address on a dual-stack listener counts
as this machine, so a browser on `127.0.0.1` works against a `[::]` bind.

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
you send or dismiss some. Once that budget is full, the daemon refuses another
upload from its headers, before it reads the bytes, so a full queue cannot be
used to make the machine buffer a large file.

A waiting message's files live in the daemon's data directory under
`guest/<request-id>/`. Dismissing the message, ending the link, creating a new
link, settling the thread, the 24-hour expiry, or turning the Labs setting off
deletes them. Ending access and deleting files are separate steps: the link row
is removed first, so a disk error can never leave a link live, and the periodic
sweep retries the file cleanup. A message you send keeps its files like any
other message in the thread.

The guest's text and files run with the thread's current permission mode once
you send them. Check them first, as you would anything pasted from a chat.

Over Connect a message's files are never uploaded before the daemon agrees to
the exchange. A current daemon marks its relay socket as one that sends a hello,
then says on that hello that it admits uploads. Until that hello arrives the
socket is unknown, and Connect never streams to an unknown socket: the guest
gets a retryable busy answer and the exchange is cancelled, so a hello that
arrives late costs a retry, never an unaccepted body. A hello wakes every
request waiting for it, so two uploads at once both learn what the daemon
speaks. Once the hello names admission, Connect asks first and streams no body
until that answer arrives; a refused message is answered without reading a byte
of its body, and a current daemon that goes quiet is an error whose slot is
released by a cancel rather than at the idle timeout. The whole exchange is
pinned to the one relay socket it started on: capability, admission, the request
and every chunk. If the daemon reconnects mid-upload, the exchange fails with a
retryable answer instead of streaming to a socket whose hello and admission are
unknown, and the capability is read once more as the deadline passes so a hello
that arrives in that instant is still an answer. A socket opened by a build
that predates hellos, or a hello that omits admission, keeps the bounded legacy
path and that build's short pre-authorization buffer. The daemon holds a
separate upload slot from the moment it admits the body until the body is
complete, so a browser that trickles an upload never takes the slot a state read
or a snapshot needs, and a browser that disappears gives its slot back at the
next ping. A cancel releases that slot at once, even while the link check or the
relay's outbound queue is still busy, and the admitted state is written before
the go-ahead leaves, so the pre-authorization cap covers only bytes that arrived
before authorization; a refusal gives the slot back before its answer waits for
queue room. A chunk that waited for queue capacity is checked against its pinned
connection once more as it is enqueued, so it can never enter a socket the
daemon has replaced. A session belongs to the sockets that opened it: when the
daemon reconnects, the replaced socket still closes its own sessions and tells
the other device, and its frames can no longer reach the replacement under the
old session. The replacement's presence is not overwritten by the old socket's
close: presence updates are serialised per device and check the connection they
belong to as they are written. A completed body keeps its slot until its answer
actually holds a work slot, so uploads cannot pile up past the cap while every
work slot is busy.

## What the daemon enforces

The guest page is a small HTTP surface, not the device WebSocket protocol. Every
request carries the link token; the daemon hashes it, compares in constant time,
checks the 24-hour expiry, checks the browser cookie and then checks that the
thread named by the link still has a live link and is not settled. The client
never sends a thread id, so there is nothing to tamper with. A message
submission checks the token and binding from its headers before any of the body
is read, requires a same-origin JSON POST, reserves the pending quota before it
writes any file, and publishes the request only after the files are written. A
request is written in two steps: a row that holds the quota and its files, then
the card above your composer. Startup finishes or discards a request interrupted
between them, so there is never a row that uses quota without a card. The card
and the placement it causes exist only once the journal holds them: a failed
append restores the state you last saw, so a publication that never happened
leaves no pending count behind. The card is published only once its row says
`pending`, the state Send and Dismiss claim, so a card you can see can always
be answered; a failed last step publishes nothing and leaves the row for the
sweep, which finishes it or discards it with its files when the link is gone,
and a snapshot hides a journaled card whose row is not answerable yet. When the
sweep finishes such a row it puts the request back in the thread (so it counts
as waiting and is not auto-settled away) and announces the card again under a
fresh sequence: a live owner's client drops a frame at or below the last
sequence it applied, so the original copy would never reach it. The guest's
page keeps the newest copy of a request's card and shows it once. A thread's
append, its guest row write and its broadcast happen as one ordered step under
the daemon's append lock, so an event from another thread can never overtake
one of them in a client's stream, and a replay can never hand out a card before
its row is answerable. A Send
or reservation interrupted by a shutdown
is answered from the thread itself: the message carries the request's id in its
item id, so recovery asks the journal before it decides anything about the link.
A message that reached the thread resolves as sent, even when the link expired
while the daemon was down; a row claimed before the append, whose message never
arrived, resolves as dismissed and its files are removed. The item id travels
with the message through the thread's queue, so a Send that arrives while the
agent is working waits in the queue under the same id, is promoted under it, and
a restart while it waits, while it may have been delivered or after it was taken
still answers sent, never dismissed. If that waiting prompt is shown as **May
have been delivered** and you choose **Send again**, the second delivery is a
message of its own with a fresh item id, so the transcript shows both rather
than hiding the second behind the first. A prompt whose append failed is not queued
for delivery, so retrying a Send delivers it once, and the
thread is left exactly as you last saw it: not Working with a turn nothing will
deliver, and keeping the pause and auto-resume a limit had armed. A Send
also records the attachment references it takes for the thread, so a crash
between that step and the append releases exactly those references and the
per-thread copy, without touching a file an earlier message of the same thread
already uses. In both cases its resolution is raised and the row stays while the
link lives, so the same browser's next poll clears its card; a row whose link is
already gone is dropped once its answer is raised. A refused
submission whose files cannot be removed keeps a cleanup record until the sweep
retries, rather than losing the only row that names those files. If the
link ended, expired, the Labs switch turned off or the thread settled while the
files were being written, publication is refused and the files and row are
deleted. The last placement check and the card's append happen under the same
thread record lock, so a settle commits either before the check (and the card is
refused) or after the card (and the settle hook resolves it); a settled thread
never keeps an unanswerable card.

A state read is answered under the link that allowed it, and the link is checked
again after the journal was read. Stopping or replacing the link while a poll is
in flight refuses that poll instead of delivering events recorded after your
decision.

Send, Dismiss and every cleanup pass claim a request with one conditional
update, so a message can never be both sent and dismissed, and the sweep can
never pull the files out from under a Send that already claimed them, even when
it read the row just before that Send claimed it. A finished request keeps its
row until its link ends, so a guest that polls after you dismissed its message
still learns that the card resolved, and while the link lives a sweep also
retries removing files a refused disk left behind. The answer is durable: if writing its event
fails, the row records that it is still owed, and the sweep, a restart and Stop
sharing all re-emit it before the row can go. The row's marker is written only
after the event is durable, so a crash never leaves a row that claims an answer
nobody received; startup compares every finished request with the journal,
before it ends any link, and raises an answer that is missing — including one an
older build's marker claimed but never appended, whose link expired while the
daemon was down. Each answer is journaled once: writers take
turns, and the writer asks the thread's journal before it appends, so a Dismiss
and a sweep that both hold the finished row cannot each emit it and a restart
never appends one the journal already holds. That question is answered by the
request id, so an answer costs one lookup rather than a read of the thread's
whole event log. Ending the link only drops a
finished row once both its files are gone and its answer is on the wire; anything
else stays a cleanup record for the next sweep. A submission or a Send runs to
completion on the daemon while it keeps running, even if the guest's browser
disconnects or the owner's connection drops while it is writing, so a dropped
connection cannot strand a half-written
request and its quota. The connection runs each request to completion, so a
closed socket never cancels a step that already changed the record, and the
guest submission, Send and Dismiss each continue on their own task. A Send or
Dismiss writes its audit line there too, so the owner's disconnect after the
claim still leaves a record; if the daemon itself stops, the startup sweep
resolves the interrupted row instead. Turning the Labs switch off saves the
setting and ends every link in one transaction, so re-enabling it cannot bring
a link back.

A guest request can only read that thread's events, submit a waiting message,
and fetch an attachment that belongs to a message in that thread. Events are
projected through an allow-list, so the guest never sees the workspace path,
approvals, filesystem events, subagent threads or MCP administration. Tool calls
lose their raw arguments, and a file-editing tool loses its output too, so a
Codex file change's diff or a patch never reaches the guest. Everything else —
another thread, a diff, a file, a terminal, an approval, a model change — has no
route. A guest link carries its own token and no device identity, so pairing has
no guest scope to grant.

Only the machine's owner, or a paired device with the admin scope, may copy a
link's URL; a view-only device sees that a link exists and when it expires, but
the URL comes back `null`. A paired device's read of the URL is written to the
daemon's remote audit trail with the thread it belongs to. Reading the URL on
the machine itself is the owner acting locally, not a remote action, so it is
not recorded there.

Through Connect, the guest page itself is served by Connect from its own fixed,
hash-checked CSP document; Connect never relays a daemon's HTML. Only JSON and
an allow-list of media types come back from the daemon, and Connect rebuilds the
headers it passes on, including the binding cookie's path, which must name the
public `/guest/<machine>/<token>` prefix. Every relayed answer is sandboxed with
`Content-Security-Policy: sandbox; default-src 'none'`, and a PDF or opaque file
is sent as a download instead of opening in Connect's tab. Request and response
bodies travel in bounded 512 KB frames, so a 10 MB upload never grows a shared
relay socket frame; Connect refuses an exchange over the guest limit before it
reaches the socket. A daemon announces on its relay hello that it admits uploads,
and Connect waits for that admission frame before an upload's first chunk, so a
request the daemon will refuse costs no body frames. A daemon that does not
announce it keeps the older short wait and its own bounded buffer. If a relayed
exchange fails or its answer times out, Connect forgets both its waiter and any
half-arrived chunked body. Guest routes are traced by their template
(`/guest/{token}/state`), never by the raw URI, so the token does not appear in
logs.
