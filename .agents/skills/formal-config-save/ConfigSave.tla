------------------------------- MODULE ConfigSave -------------------------------
(* Concurrent writers of the desktop config, as crates/thalvi-app/src/settings.rs
   (Store::save, write_atomically, write_private), commands.rs (save),
   app.rs (AppState::remember, AppState::apply) and agent.rs (settings_set) do it.
   Each action is one file-system call, one lock of AppState::config, or one
   channel send. SKILL.md maps them. *)
EXTENDS Naturals, Sequences

CONSTANTS FormSaves, SwitchFlips, MainThread,
          Serialized, SnapshotOutsideLock, AgentMergeOutsideLock, ApplyOutsideLock,
          RenameBeforeWrite

Ops == FormSaves \cup SwitchFlips
MaxInode == 1 + 3          \* the file at start, then at most one temporary per writer
\* The form has one field per form writer; a form save sets its own field and carries the
\* others over from the config it read (settings.rs `settings_json`, agent.rs `plan_over`).
Fields == {"look", "mic"}
FieldOf(o) == IF o = "A" THEN "mic" ELSE "look"   \* the Settings save sets the look, the agent the microphone
Initial == [form |-> [f \in Fields |-> "f0"], switch |-> FALSE]
Empty == [form |-> [f \in Fields |-> "empty"], switch |-> FALSE]   \* created, nothing written yet
Unused == [form |-> [f \in Fields |-> "none"], switch |-> FALSE]

VARIABLES
    pc,         \* index of each writer's next step in Flow(o)
    failed,     \* the writer's save returned an error
    mem,        \* AppState::config
    engine,     \* the form the engine got last through Control::Apply
    path,       \* inode desktop.json names
    tmp,        \* inode desktop.json.tmp names, 0 for none
    inodes,     \* inode -> content, Empty or Unused
    nextInode,
    fd,         \* the inode each writer opened
    content,    \* what each writer validated and serialized
    lock,       \* who holds the one lock of the Serialized design, or "nobody"
    lastField,   \* per form field, the writer of the last form save that set it and reported success
    lastSwitch  \* the switch of the last switch flip that reported success

vars == <<pc, failed, mem, engine, path, tmp, inodes, nextInode, fd, content,
          lock, lastField, lastSwitch>>

\* The agent's `settings_set` plans inside `SyncedConfig::save_form`'s lock; the mutation plans
\* first, as it did before the repair.
ReadsBeforeLock(o) == SnapshotOutsideLock \/ (AgentMergeOutsideLock /\ o = "A")

Flow(o) ==
    (IF Serialized /\ ~ReadsBeforeLock(o) THEN <<"lock">> ELSE <<>>)
    \o <<"snap">>
    \o (IF Serialized /\ ReadsBeforeLock(o) THEN <<"lock">> ELSE <<>>)
    \o (IF RenameBeforeWrite THEN <<"rmtmp", "create", "rename", "write">>
                             ELSE <<"rmtmp", "create", "write", "rename">>)
    \o (IF Serialized /\ ApplyOutsideLock THEN <<"unlock">> ELSE <<>>)
    \o <<"apply">>
    \o (IF o \in FormSaves THEN <<"send">> ELSE <<>>)
    \o (IF Serialized /\ ~ApplyOutsideLock THEN <<"unlock">> ELSE <<>>)

Step(o) == IF pc[o] <= Len(Flow(o)) THEN Flow(o)[pc[o]] ELSE "done"
Finished(o) == Step(o) = "done" \/ failed[o]
Started(o) == pc[o] > 1 \/ failed[o]

\* Settings saves and tray switches run on the main thread, one at a time.
MayRun(o) ==
    /\ ~Finished(o)
    /\ (pc[o] = 1 /\ o \in MainThread) =>
          \A other \in MainThread \ {o} : ~Started(other) \/ Finished(other)

Advance(o) == pc' = [pc EXCEPT ![o] = pc[o] + 1]

Fail(o) ==
    /\ failed' = [failed EXCEPT ![o] = TRUE]
    /\ lock' = IF lock = o THEN "nobody" ELSE lock

Init ==
    /\ pc = [o \in Ops |-> 1]
    /\ failed = [o \in Ops |-> FALSE]
    /\ mem = Initial
    /\ engine = Initial.form
    /\ path = 1
    /\ tmp = 0
    /\ inodes = [i \in 1..MaxInode |-> IF i = 1 THEN Initial ELSE Unused]
    /\ nextInode = 2
    /\ fd = [o \in Ops |-> 0]
    /\ content = [o \in Ops |-> Initial]
    /\ lock = "nobody"
    /\ lastField = [f \in Fields |-> "f0"]
    /\ lastSwitch = Initial.switch

Lock(o) ==
    /\ MayRun(o)
    /\ Step(o) = "lock" /\ lock = "nobody"
    /\ lock' = o
    /\ Advance(o)
    /\ UNCHANGED <<failed, mem, engine, path, tmp, inodes, nextInode, fd, content,
                   lastField, lastSwitch>>

Unlock(o) ==
    /\ MayRun(o)
    /\ Step(o) = "unlock"
    /\ lock' = "nobody"
    /\ Advance(o)
    /\ UNCHANGED <<failed, mem, engine, path, tmp, inodes, nextInode, fd, content,
                   lastField, lastSwitch>>

\* SyncedConfig::save_form reads the config in use and builds the form from it: the agent
\* merges its patch over it (`plan_over`), the Settings save sets its field; AppState::remember
\* reads the whole config and changes its switch.
Snap(o) ==
    /\ MayRun(o)
    /\ Step(o) = "snap"
    /\ content' = [content EXCEPT ![o] =
                     IF o \in FormSaves
                     THEN [form |-> [f \in Fields |-> IF f = FieldOf(o) THEN o ELSE mem.form[f]],
                           switch |-> mem.switch]
                     ELSE [form |-> mem.form, switch |-> TRUE]]
    /\ Advance(o)
    /\ UNCHANGED <<failed, mem, engine, path, tmp, inodes, nextInode, fd, lock,
                   lastField, lastSwitch>>

\* write_private: `let _ = std::fs::remove_file(path)` on the shared temporary name.
RmTmp(o) ==
    /\ MayRun(o)
    /\ Step(o) = "rmtmp"
    /\ tmp' = 0
    /\ Advance(o)
    /\ UNCHANGED <<failed, mem, engine, path, inodes, nextInode, fd, content, lock,
                   lastField, lastSwitch>>

\* write_private: open with create_new; it fails if the name exists, and
\* write_atomically then removes the temporary name, whoever's it is.
Create(o) ==
    /\ MayRun(o)
    /\ Step(o) = "create"
    /\ IF tmp = 0
       THEN /\ tmp' = nextInode
            /\ inodes' = [inodes EXCEPT ![nextInode] = Empty]
            /\ fd' = [fd EXCEPT ![o] = nextInode]
            /\ nextInode' = nextInode + 1
            /\ Advance(o)
            /\ UNCHANGED <<failed, lock>>
       ELSE /\ tmp' = 0
            /\ Fail(o)
            /\ UNCHANGED <<pc, inodes, fd, nextInode>>
    /\ UNCHANGED <<mem, engine, path, content, lastField, lastSwitch>>

\* write_all through the open file: it reaches the inode wherever it is linked now.
Write(o) ==
    /\ MayRun(o)
    /\ Step(o) = "write"
    /\ inodes' = [inodes EXCEPT ![fd[o]] = content[o]]
    /\ Advance(o)
    /\ UNCHANGED <<failed, mem, engine, path, tmp, nextInode, fd, content, lock,
                   lastField, lastSwitch>>

\* std::fs::rename(&temporary, path): moves whatever the name holds now.
Rename(o) ==
    /\ MayRun(o)
    /\ Step(o) = "rename"
    /\ IF tmp # 0
       THEN /\ path' = tmp
            /\ tmp' = 0
            /\ Advance(o)
            /\ UNCHANGED <<failed, lock>>
       ELSE /\ Fail(o)
            /\ UNCHANGED <<pc, path, tmp>>
    /\ UNCHANGED <<mem, engine, inodes, nextInode, fd, content, lastField, lastSwitch>>

\* AppState::apply (form saves) or AppState::remember: `*config = ...` under its lock.
\* A save that gets here returns Ok.
Apply(o) ==
    /\ MayRun(o)
    /\ Step(o) = "apply"
    /\ mem' = content[o]
    /\ lastField' = IF o \in FormSaves THEN [lastField EXCEPT ![FieldOf(o)] = o] ELSE lastField
    /\ lastSwitch' = IF o \in SwitchFlips THEN content[o].switch ELSE lastSwitch
    /\ Advance(o)
    /\ UNCHANGED <<failed, engine, path, tmp, inodes, nextInode, fd, content, lock>>

\* AppState::apply's `self.link.send(Control::Apply(..))`; the runner applies them in order.
Send(o) ==
    /\ MayRun(o)
    /\ Step(o) = "send"
    /\ engine' = content[o].form
    /\ Advance(o)
    /\ UNCHANGED <<failed, mem, path, tmp, inodes, nextInode, fd, content, lock,
                   lastField, lastSwitch>>

Quiet == \A o \in Ops : Finished(o)

Done == Quiet /\ UNCHANGED vars

Next ==
    \/ \E o \in Ops : Lock(o)
    \/ \E o \in Ops : Unlock(o)
    \/ \E o \in Ops : Snap(o)
    \/ \E o \in Ops : RmTmp(o)
    \/ \E o \in Ops : Create(o)
    \/ \E o \in Ops : Write(o)
    \/ \E o \in Ops : Rename(o)
    \/ \E o \in Ops : Apply(o)
    \/ \E o \in Ops : Send(o)
    \/ Done

Spec == Init /\ [][Next]_vars

File == inodes[path]

\* Store::save "replaces the file in one step": the file is always a whole save.
FileAlwaysWhole == \A f \in Fields : File.form[f] \notin {"empty", "none"}

\* Once every save has returned, the running app, its engine and the file agree.
RunningMatchesFile == Quiet => (File = mem /\ engine = mem.form)

\* Once every save has returned, each form field holds the value of the last reported save
\* that set it, so a save that sets one field never undoes another's; the switch holds the last
\* reported switch: a Save never writes the switch, a switch never writes the form
\* (DECISIONS.md, "Packaged desktop with an OS-held login entry").
EachFieldKeepsItsLastSave ==
    Quiet => (\A f \in Fields : File.form[f] = lastField[f]) /\ File.switch = lastSwitch
================================================================================
