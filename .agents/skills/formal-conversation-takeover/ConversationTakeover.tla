------------------------- MODULE ConversationTakeover -------------------------
(* One saved conversation, opened by several connections in turn, as
   crates/thalvi-server/src/store.rs keeps it. Each action is one lock
   acquisition of SessionStore::state or one await point; SKILL.md maps them. *)
EXTENDS Naturals

CONSTANTS Conns, None, MaxTurns,
          RevokeOnlyPrevious, DeleteSparesPredecessors,
          ReadBeforeRevoke, ReleaseBeforeLastSave

VARIABLES
    exists,      \* State::sessions holds the conversation
    stored,      \* which save the stored turns came from; 0 is what was on disk
    nextVer,
    liveOwner,   \* the connection whose Live entry is in State::live, or None
    livePreds,   \* that entry's Live::predecessors
    dead,        \* each Attachment's dead flag
    released,    \* each Attachment's released sender has fired
    pc,
    prev,        \* the Live entry a connection's attach removed
    preds,       \* the predecessors its attach carries
    order,       \* order of the attaches' first lock
    nextOrder,
    base,        \* the save a connection's in-memory history extends
    unsaved,     \* turns not stored yet: Saved::sync saves only when the history grew
    turns,
    killed,      \* marked dead by a delete
    expired,     \* the takeover that waited for this connection passed HANDOFF_WAIT
    returned,    \* attach has returned
    stale,       \* ghost: a save landed from a connection older than a returned one
    clobbered,   \* ghost: a save replaced turns its connection never read
    gone         \* ghost: a delete removed the conversation

vars == <<exists, stored, nextVer, liveOwner, livePreds, dead, released, pc,
          prev, preds, order, nextOrder, base, unsaved, turns, killed,
          expired, returned, stale, clobbered, gone>>

saveVars == <<exists, stored, nextVer, base, unsaved, stale, clobbered>>

Init ==
    /\ exists = TRUE
    /\ stored = 0
    /\ nextVer = 1
    /\ liveOwner = None
    /\ livePreds = {}
    /\ dead = [c \in Conns |-> FALSE]
    /\ released = [c \in Conns |-> FALSE]
    /\ pc = [c \in Conns |-> "idle"]
    /\ prev = [c \in Conns |-> None]
    /\ preds = [c \in Conns |-> {}]
    /\ order = [c \in Conns |-> 0]
    /\ nextOrder = 1
    /\ base = [c \in Conns |-> None]
    /\ unsaved = [c \in Conns |-> FALSE]
    /\ turns = [c \in Conns |-> 0]
    /\ killed = [c \in Conns |-> FALSE]
    /\ expired = [c \in Conns |-> FALSE]
    /\ returned = [c \in Conns |-> FALSE]
    /\ stale = FALSE
    /\ clobbered = FALSE
    /\ gone = FALSE

\* SessionStore::save: one lock; a dead attachment changes nothing.
DoSave(c) ==
    IF dead[c]
    THEN UNCHANGED saveVars
    ELSE /\ exists' = TRUE
         /\ stored' = nextVer
         /\ nextVer' = nextVer + 1
         /\ base' = [base EXCEPT ![c] = nextVer]
         /\ unsaved' = [unsaved EXCEPT ![c] = FALSE]
         /\ clobbered' = (clobbered \/ (exists /\ stored # base[c]))
         /\ stale' = (stale \/ \E d \in Conns : returned[d] /\ order[d] > order[c])

\* attach, first lock: a missing id gets a fresh conversation; otherwise the
\* old Live entry is removed and this one inserted, carrying every predecessor.
Claim(c) ==
    /\ pc[c] = "idle"
    /\ IF ~exists
       THEN /\ pc' = [pc EXCEPT ![c] = "elsewhere"]
            /\ UNCHANGED <<liveOwner, livePreds, prev, preds, order, nextOrder>>
       ELSE LET p == liveOwner
                ps == IF p = None THEN {} ELSE livePreds \cup {p}
            IN /\ liveOwner' = c
               /\ livePreds' = ps
               /\ prev' = [prev EXCEPT ![c] = p]
               /\ preds' = [preds EXCEPT ![c] = ps]
               /\ order' = [order EXCEPT ![c] = nextOrder]
               /\ nextOrder' = nextOrder + 1
               /\ pc' = [pc EXCEPT ![c] = IF p = None THEN "read" ELSE "waiting"]
    /\ UNCHANGED <<exists, stored, nextVer, dead, released, base, unsaved,
                   turns, killed, expired, returned, stale, clobbered, gone>>

AfterWait == IF ReadBeforeRevoke THEN "read" ELSE "revoke"

\* attach: Moved sent, then previous.released resolves.
WaitReleased(c) ==
    /\ pc[c] = "waiting"
    /\ released[prev[c]]
    /\ pc' = [pc EXCEPT ![c] = AfterWait]
    /\ UNCHANGED <<exists, stored, nextVer, liveOwner, livePreds, dead, released,
                   prev, preds, order, nextOrder, base, unsaved, turns, killed,
                   expired, returned, stale, clobbered, gone>>

\* attach: HANDOFF_WAIT passes. Every takeover that started earlier has the same
\* wait and an earlier deadline, so those deadlines have passed too.
WaitTimeout(c) ==
    /\ pc[c] = "waiting"
    /\ pc' = [pc EXCEPT ![c] = AfterWait]
    /\ expired' = [x \in Conns |-> expired[x] \/
                    \E w \in Conns : pc[w] = "waiting" /\ order[w] <= order[c] /\ prev[w] = x]
    /\ UNCHANGED <<exists, stored, nextVer, liveOwner, livePreds, dead, released,
                   prev, preds, order, nextOrder, base, unsaved, turns, killed,
                   returned, stale, clobbered, gone>>

\* attach, second lock: mark every predecessor dead; clear them if still the owner.
Revoke(c) ==
    /\ pc[c] = "revoke"
    /\ dead' = [x \in Conns |-> dead[x] \/
                 (IF RevokeOnlyPrevious THEN x = prev[c] ELSE x \in preds[c])]
    /\ livePreds' = IF liveOwner = c THEN {} ELSE livePreds
    /\ pc' = [pc EXCEPT ![c] = IF ReadBeforeRevoke THEN "attached" ELSE "read"]
    /\ returned' = IF ReadBeforeRevoke THEN [returned EXCEPT ![c] = TRUE] ELSE returned
    /\ UNCHANGED <<exists, stored, nextVer, liveOwner, released, prev, preds, order,
                   nextOrder, base, unsaved, turns, killed, expired, stale, clobbered, gone>>

\* attach, third lock: read the stored history (empty if it was deleted meanwhile).
Read(c) ==
    /\ pc[c] = "read"
    /\ base' = [base EXCEPT ![c] = IF exists THEN stored ELSE None]
    /\ LET next == IF ReadBeforeRevoke /\ prev[c] # None THEN "revoke" ELSE "attached"
       IN /\ pc' = [pc EXCEPT ![c] = next]
          /\ returned' = IF next = "attached" THEN [returned EXCEPT ![c] = TRUE] ELSE returned
    /\ UNCHANGED <<exists, stored, nextVer, liveOwner, livePreds, dead, released, prev,
                   preds, order, nextOrder, unsaved, turns, killed, expired, stale, clobbered, gone>>

\* handle_session: a turn adds to the in-memory history.
Turn(c) ==
    /\ pc[c] = "attached"
    /\ turns[c] < MaxTurns
    /\ turns' = [turns EXCEPT ![c] = turns[c] + 1]
    /\ unsaved' = [unsaved EXCEPT ![c] = TRUE]
    /\ UNCHANGED <<exists, stored, nextVer, liveOwner, livePreds, dead, released, pc,
                   prev, preds, order, nextOrder, base, killed, expired, returned,
                   stale, clobbered, gone>>

\* handle_session loop top: Persistence::sync.
Sync(c) ==
    /\ pc[c] = "attached"
    /\ unsaved[c]
    /\ DoSave(c)
    /\ UNCHANGED <<liveOwner, livePreds, dead, released, pc, prev, preds, order,
                   nextOrder, turns, killed, expired, returned, gone>>

\* The loop ends: detached (Moved or Deleted) or the client went away.
Close(c) ==
    /\ pc[c] = "attached"
    /\ pc' = [pc EXCEPT ![c] = IF ReleaseBeforeLastSave THEN "releasing" ELSE "closing"]
    /\ UNCHANGED <<exists, stored, nextVer, liveOwner, livePreds, dead, released,
                   prev, preds, order, nextOrder, base, unsaved, turns, killed,
                   expired, returned, stale, clobbered, gone>>

\* Persistence::finish, first half: the last sync.
FinalSave(c) ==
    /\ pc[c] = "closing"
    /\ IF unsaved[c] THEN DoSave(c) ELSE UNCHANGED saveVars
    /\ pc' = [pc EXCEPT ![c] = IF ReleaseBeforeLastSave THEN "done" ELSE "releasing"]
    /\ UNCHANGED <<liveOwner, livePreds, dead, released, prev, preds, order,
                   nextOrder, turns, killed, expired, returned, gone>>

\* Persistence::finish, second half: SessionStore::release.
Release(c) ==
    /\ pc[c] = "releasing"
    /\ released' = [released EXCEPT ![c] = TRUE]
    /\ IF liveOwner = c
       THEN liveOwner' = None /\ livePreds' = {}
       ELSE UNCHANGED <<liveOwner, livePreds>>
    /\ pc' = [pc EXCEPT ![c] = IF ReleaseBeforeLastSave THEN "closing" ELSE "done"]
    /\ UNCHANGED <<exists, stored, nextVer, dead, prev, preds, order, nextOrder,
                   base, unsaved, turns, killed, expired, returned, stale, clobbered, gone>>

\* write_loop, DiskOp::Remove: one lock while the file is removed.
Delete ==
    /\ exists
    /\ LET hit == {x \in Conns : x = liveOwner \/
                    (~DeleteSparesPredecessors /\ x \in livePreds)}
       IN /\ dead' = [x \in Conns |-> dead[x] \/ x \in hit]
          /\ killed' = [x \in Conns |-> killed[x] \/ x \in hit]
    /\ exists' = FALSE
    /\ gone' = TRUE
    /\ liveOwner' = None
    /\ livePreds' = {}
    /\ UNCHANGED <<stored, nextVer, released, pc, prev, preds, order, nextOrder,
                   base, unsaved, turns, expired, returned, stale, clobbered>>

Finished == /\ \A c \in Conns : pc[c] \in {"done", "elsewhere"}
            /\ UNCHANGED vars

Next ==
    \/ \E c \in Conns : Claim(c) \/ WaitReleased(c) \/ WaitTimeout(c) \/ Revoke(c)
                        \/ Read(c) \/ Turn(c) \/ Sync(c) \/ Close(c)
                        \/ FinalSave(c) \/ Release(c)
    \/ Delete
    \/ Finished

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ liveOwner \in Conns \cup {None}
    /\ livePreds \subseteq Conns
    /\ pc \in [Conns -> {"idle", "waiting", "revoke", "read", "attached",
                         "closing", "releasing", "done", "elsewhere"}]

\* Once a takeover has returned, no connection that opened the conversation before it saves.
NoStaleSave == ~stale

\* A save never replaces stored turns that its connection had not read.
NoLostUpdate == ~clobbered

\* A deleted conversation never comes back.
NoResurrection == gone => ~exists

\* A connection that closed with turns unsaved lost them only to a delete or a
\* takeover that stopped waiting for it.
HandoffKeepsTurns ==
    \A c \in Conns : (pc[c] = "done" /\ unsaved[c]) => (killed[c] \/ expired[c])
================================================================================
