"""The Rust examples under Pointman core's own Python node: its host (ExtensionHost, what a node runs)
and its stand-in (mb_extension_testing), as core's tests run Python extensions. tests/python.rs runs
it with core's source on PYTHONPATH:

    host_check.py <hello package> <demo package> <state dir>

Each package is laid out as a node installs it: extension.toml, and the binary at bin/<id>."""

import sys
import threading
import time
from pathlib import Path

import messageboard
from messageboard.extensions.host import ExtensionError, ExtensionHost
from messageboard.extensions.manifest import load

try:
    from pointman_extension_testing import ExtensionFailed, StandInNode
except ImportError:
    from mb_extension_testing import ExtensionFailed, StandInNode

HELLO, DEMO, STATE = (Path(p) for p in sys.argv[1:4])
VAULT = {"demo.work": "w", "demo.spare": "s", "shared.key": "k", "other.key": "o"}


def raises(fn, error, **attrs):
    try:
        fn()
    except error as e:
        for k, v in attrs.items():
            got = str(e) if k == "message" else getattr(e, k)
            assert (v in got) if k == "message" else got == v, f"{k}: {got!r}, wanted {v!r}"
        return e
    raise AssertionError(f"no {error.__name__}")


def wait_for(cond, timeout=10.0):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if cond():
            return True
        time.sleep(0.05)
    return False


def host(root, name, **kw):
    h = ExtensionHost(root, node="test-node", machine="test-machine", state_dir=STATE / name, **kw)
    h.start()
    assert h.wait_ready(), h.error
    return h


def manifests():
    for root in (HELLO, DEMO):
        m = load(root)
        assert m.run.runtime == "binary" and m.run.command == [f"bin/{m.extension.id}"], m.run


def host_hello():
    h = host(HELLO, "hello", settings={"greeting": "Hi"})
    try:
        seen = []
        res = h.run("hello.greet", {"name": "James"}, progress=lambda f, stage, detail: seen.append((f, stage)))
        assert res.result == {"text": "Hi, James!"}, res.result
        assert res.main("text").path.read_text() == "Hi, James!\n"
        assert seen == [(0.5, "writing")], seen
        assert h.events["hello.last"] == {"name": "James"}
        assert h.status()["state"] == "running"
        h.change_settings({})
        assert h.run("hello.greet", {"name": "Ada"}).result == {"text": "Hello, Ada!"}
        raises(lambda: h.run("hello.greet", {}), ExtensionError, message="'name' is a required property")
        raises(lambda: h.run("hello.nope", {}), ExtensionError, message="isn't a command")
    finally:
        h.stop()
    assert h.state == "off"


def host_demo():
    h = host(DEMO, "demo", settings={"limit": 5}, secrets=VAULT.__getitem__, secret_names=lambda prefix: list(VAULT),
             methods={"demo.lookup": lambda p: {"found": p}})
    try:
        assert wait_for(lambda: "demo.started" in h.events and "demo.tick" in h.events), h.events
        assert h.events["demo.started"]["settings"] == {"label": "demo", "limit": 5}
        assert h.events["demo.started"]["node"]["machine"] == "test-machine"
        assert h.run("demo.secret", {"name": "demo.work", "prefix": "demo."}).result == \
            {"value": "w", "names": ["demo.spare", "demo.work"]}
        assert h.run("demo.secret", {"prefix": ""}).result == {"names": ["demo.spare", "demo.work", "shared.key"]}
        raises(lambda: h.run("demo.secret", {"name": "other.key"}), ExtensionError, retry=False,
               message="may not use the secret 'other.key'")
        raises(lambda: h.run("demo.fail", {"how": "retry", "message": "later"}), ExtensionError, retry=True, message="later")
        raises(lambda: h.run("demo.fail", {"message": "no"}), ExtensionError, retry=False, message="no")
        t0 = time.monotonic()
        raises(lambda: h.run("demo.step", {"steps": 200, "step_ms": 20}, cancelled=lambda: time.monotonic() - t0 > 0.5),
               ExtensionError, cancelled=True)
        done = h.run("demo.step", {"steps": 3})
        assert done.result["from"] == 0 and done.result["to"] == 3
        assert h.call("demo.ask", {"q": 1})["params"] == {"q": 1}
        assert h.call("demo.relay", {"method": "demo.lookup", "params": {"x": 1}}) == {"found": {"x": 1}}
        # a crash fails what's running with retry, and the node starts it again
        raises(lambda: h.run("demo.fail", {"how": "exit"}), ExtensionError, retry=True)
        assert h.wait_ready(15), h.error
        assert h.run("demo.echo", {"again": True}).result["params"] == {"again": True}
    finally:
        h.stop()


def standin_hello():
    with StandInNode(HELLO, settings={"greeting": "Hi"}, state_dir=STATE / "standin") as node:
        done = node.run("hello.greet", {"name": "James"})
        assert done.result == {"text": "Hi, James!"}
        assert Path(done.outputs[0]["path"]).read_text() == "Hi, James!\n"
        assert node.events["hello.last"] == {"name": "James"}
    with StandInNode(HELLO) as node:
        assert node.run("hello.greet", {"name": "Ada"}).result == {"text": "Hello, Ada!"}
        raises(lambda: node.run("hello.greet", {}), ExtensionFailed, message="name is missing")


def standin_demo():
    with StandInNode(DEMO, secrets=VAULT) as node:
        assert node.run("demo.secret", {"name": "shared.key"}).result == {"value": "k"}
        raises(lambda: node.run("demo.secret", {"name": "other.key"}), ExtensionFailed, message="may not use")
        result = {}
        t = threading.Thread(target=lambda: result.update(e=raises(
            lambda: node.run("demo.step", {"steps": 200, "step_ms": 20}, cid="long"), ExtensionFailed)))
        t.start()
        assert wait_for(lambda: node.cancel("long") == {"found": True}), "it never had the command"
        t.join()
        assert result["e"].cancelled
        assert node.call("demo.ask", {"q": 2})["params"] == {"q": 2}


print(f"core: {Path(messageboard.__file__).parent}", flush=True)
for check in (manifests, host_hello, host_demo, standin_hello, standin_demo):
    check()
    print(f"ok  {check.__name__}", flush=True)
