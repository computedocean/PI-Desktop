import assert from "node:assert/strict";
import test from "node:test";
import { browser, createLanServer, desktop } from "./helpers/lan-webui-fixture.mjs";

/** A paired phone against the real HTTP listener and a fake Desktop. */
async function setup(t) {
  const fake = desktop();
  fake.sessions.push({ id: "s-a2", title: "Second chat", projectPath: "/work/a", mode: "agent", updatedAt: "2026-11-04" });
  const server = createLanServer({ handler: fake.workspace, assets: new Map([["/index.html", { type: "text/html", body: "<p>ok</p>" }]]) });
  const { port } = await server.start("127.0.0.1", 0);
  t.after(async () => {
    await server.stop();
    fake.workspace.dispose();
  });
  const phone = browser(`http://127.0.0.1:${port}`);
  const { value: code } = server.issueCode();
  assert.equal((await phone.request("POST", "/pair", { code, label: "Phone" })).status, 200);
  await phone.signIn();
  const bridge = fake.workspaceDeps.getBridge();
  const projectRevision = async (id, archived = false) =>
    (await phone.request("GET", `/v1/projects?q=${archived ? "&archived=1" : ""}`)).body.items.find((p) => p.id === id).revision;
  const sessionIn = async (projectId, id, archived = false) =>
    (await phone.request("GET", `/v1/sessions?projectId=${projectId}${archived ? "&archived=1" : ""}`)).body.items.find((s) => s.id === id);
  return { ...fake, phone, bridge, projectRevision, sessionIn };
}

test("an archived project keeps history readable but accepts no send or queue until restored", async (t) => {
  const { phone, turns, meta, projectRevision, sessionIn } = await setup(t);
  // A tab opened before the archive, and a child archived on its own.
  const before = await phone.request("GET", "/v1/sessions/s-a1");
  assert.equal(before.body.session.projectArchived, false);
  const child = await sessionIn("g-a", "s-a2");
  assert.equal((await phone.request("POST", "/v1/sessions/s-a2/organize", { revision: child.revision, archived: true })).status, 200);

  assert.equal((await phone.request("POST", "/v1/projects/g-a/organize", { revision: await projectRevision("g-a"), archived: true })).status, 200);

  // History stays readable; the parent's archive is not the session's own flag.
  const archived = await phone.request("GET", "/v1/sessions/s-a1");
  assert.equal(archived.status, 200);
  assert.deepEqual(archived.body.messages.items.map((m) => m.text), ["hello", "hi there", "Bash"]);
  assert.equal(archived.body.session.projectArchived, true);
  assert.equal(archived.body.session.archived, false);
  assert.equal(archived.body.session.revision, before.body.session.revision);
  assert.equal(archived.body.canSelectModel, false);
  assert.equal(meta.sessionMeta["s-a1"], undefined, "archive state is not copied to descendants");

  // A stale tab cannot send or queue.
  for (const body of [{ text: "stale send" }, { text: "stale queue", queue: true }]) {
    assert.deepEqual(await phone.request("POST", "/v1/sessions/s-a1/turns", body), { status: 409, body: { code: "ARCHIVED" } });
  }
  assert.equal(turns.length, 0, "no turn was started");

  // Restoring the project reopens its children, not a separately archived one.
  assert.equal((await phone.request("POST", "/v1/projects/g-a/organize", { revision: await projectRevision("g-a", true), archived: false })).status, 200);
  assert.equal((await phone.request("POST", "/v1/sessions/s-a1/turns", { text: "after restore" })).status, 200);
  assert.equal(turns.at(-1).params.input.text, "after restore");
  const stillArchived = await phone.request("GET", "/v1/sessions/s-a2");
  assert.equal(stillArchived.body.session.archived, true);
  assert.equal(stillArchived.body.session.projectArchived, false);
  assert.deepEqual(await phone.request("POST", "/v1/sessions/s-a2/turns", { text: "x" }), { status: 409, body: { code: "ARCHIVED" } });
  assert.equal(turns.length, 1);
});

test("web archive and send admission agree on order and never stop work", async (t) => {
  const { phone, bridge, queue, activeTurns, projectRevision, sessionIn } = await setup(t);

  // Queued work makes the project and the session busy.
  queue.set("s-a1", [{ id: "queued-1", content: "next" }]);
  assert.deepEqual(
    await phone.request("POST", "/v1/projects/g-a/organize", { revision: await projectRevision("g-a"), archived: true }),
    { status: 409, body: { code: "BUSY" } },
  );
  const own = await sessionIn("g-a", "s-a1");
  assert.deepEqual(
    await phone.request("POST", "/v1/sessions/s-a1/organize", { revision: own.revision, archived: true }),
    { status: 409, body: { code: "BUSY" } },
  );
  assert.deepEqual(queue.get("s-a1"), [{ id: "queued-1", content: "next" }], "archive never cancels work");
  queue.delete("s-a1");

  // A send already past its archive check holds off the archive...
  const start = bridge.agentHost.startTurn;
  const reached = Promise.withResolvers();
  const release = Promise.withResolvers();
  t.after(() => release.resolve());
  bridge.agentHost.startTurn = async (...args) => {
    reached.resolve();
    await release.promise;
    return start(...args);
  };
  const sending = phone.request("POST", "/v1/sessions/s-a1/turns", { text: "in flight" });
  await reached.promise;
  assert.deepEqual(
    await phone.request("POST", "/v1/projects/g-a/organize", { revision: await projectRevision("g-a"), archived: true }),
    { status: 409, body: { code: "BUSY" } },
  );
  release.resolve();
  assert.equal((await sending).status, 200);
  bridge.agentHost.startTurn = start;

  // ...and stays busy while the admitted turn runs.
  assert.ok(activeTurns.has("s-a1"));
  assert.equal(
    (await phone.request("POST", "/v1/projects/g-a/organize", { revision: await projectRevision("g-a"), archived: true })).body.code,
    "BUSY",
  );
  activeTurns.delete("s-a1");
  assert.equal((await phone.request("POST", "/v1/projects/g-a/organize", { revision: await projectRevision("g-a"), archived: true })).status, 200);
});

test("a phone turn the Host cannot hold to its permission ceiling is refused visibly", async (t) => {
  const { phone, bridge, activeTurns } = await setup(t);
  bridge.agentHost.startTurn = async () => {
    throw Object.assign(new Error("the Host could not enforce this turn's permission ceiling"), {
      errorCode: "PERMISSION_CEILING_UNAVAILABLE",
    });
  };
  assert.deepEqual(
    await phone.request("POST", "/v1/sessions/s-a1/turns", { text: "hello" }),
    { status: 503, body: { code: "PERMISSION_CEILING_UNAVAILABLE" } },
  );
  assert.equal(activeTurns.has("s-a1"), false);
});
