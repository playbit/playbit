import test from "node:test";
import assert from "node:assert/strict";
import { nativeInvitation } from "./native-handoff.ts";
import { sessionCode } from "./invitation.ts";

test("native handoff needs only a valid fragment, independently of web runtime features", () => {
    const code = sessionCode(new Uint8Array(16).fill(7));
    assert.equal(nativeInvitation("/s", `#${code}`, "public"), `playbit:session:public:${code}`);
    assert.equal(nativeInvitation("/s/", `#${code}`, "public"), `playbit:session:public:${code}`);
    for (const [path, fragment] of [["/", `#${code}`], ["/s/secret", `#${code}`], ["/s", "#bad"], ["/s", `#${code}?extra`], ["/s", "#zzzzzzzzzzzzzzzzzzzzzz"], ["/s", ""]]) {
        assert.equal(nativeInvitation(path, fragment, "public"), null);
    }
});
