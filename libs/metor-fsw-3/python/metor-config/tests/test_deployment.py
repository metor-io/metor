"""Tests for deployments: peers, copied ports, the gateway, and the golden
config file."""

from __future__ import annotations

import contextlib
import json
import os

from adcs_stub import Ctrl, Imu, LogEvent, Mode, Nav, Ping, Plant
from test_target import CRATE, ConfigTest, adcs_target, reset

import metor_config._deployment as deploy_mod
from metor_config import (
    ConfigError,
    Connect,
    Db,
    Deployment,
    Listen,
    Peer,
    Publish,
    Subscribe,
    Target,
    gateway,
)

GOLDEN = os.path.join(CRATE, "tests", "golden", "deployment.json")


def design_example() -> Deployment:
    """The design doc's three-target file, over the stub pack."""
    a = Target(cycle_rate=100.0, namespace="a")
    plant = a.add("plant", Plant())
    nav = a.add("nav", Nav(imu=plant.imu))
    a_pub = a.add("pub", Publish(transport=Listen("0.0.0.0:2240"), all=True))

    b = Target(cycle_rate=100.0, namespace="b")
    mirror = b.add("mirror", Subscribe(transport=Peer(a_pub), items=[plant, nav.est]))
    mode = b.add("mode", Mode())
    b.add("ctrl", Ctrl(est=mirror.port(nav.est), mode=mode.cmd))
    b_pub = b.add("pub", Publish(transport=Listen("0.0.0.0:2241"), all=True))

    gw, gw_db = gateway("gw", peers=[a_pub, b_pub], listen="0.0.0.0:2250")
    a.add("cmds", Subscribe(transport=Peer(gw_db), records=[Ping]))
    return Deployment(targets=[a, b, gw], hosts={"a": "10.0.0.2"})


def plant_and_peer() -> tuple[Target, Plant, Target]:
    """Target `a` with a plant, and an empty target `b`."""
    a = Target(cycle_rate=100.0, namespace="a")
    return a, a.add("plant", Plant()), Target(cycle_rate=100.0, namespace="b")


def member(namespace: str | None) -> Target:
    fsw = Target(cycle_rate=100.0, namespace=namespace)
    fsw.add("plant", Plant())
    return fsw


class EnvelopeTest(ConfigTest):
    def test_bare_target_is_deployment_of_one(self) -> None:
        bare = adcs_target().to_config()
        reset()
        wrapped = Deployment(targets=[adcs_target()]).to_config()
        self.assertEqual(deploy_mod.to_config(), wrapped)
        self.assertEqual(wrapped, {"config_version": 2, "hosts": {}, "targets": [bare]})

    def test_envelope_lists_members_in_order(self) -> None:
        a, b = member("a"), member("b")
        config = Deployment(targets=[b, a], hosts={"a": "10.0.0.2"}).to_config()
        self.assertEqual(
            [t["coordinator"]["namespace"] for t in config["targets"]], ["b", "a"]
        )
        self.assertEqual(config["hosts"], {"a": "10.0.0.2"})

    def test_deployment_of_one_needs_no_namespace(self) -> None:
        config = Deployment(targets=[member(None)]).to_config()
        self.assertIsNone(config["targets"][0]["coordinator"]["namespace"])

    def test_members_need_distinct_namespaces(self) -> None:
        with self.assertRaisesRegex(ConfigError, "names a namespace"):
            Deployment(targets=[member("a"), member(None)]).to_config()
        with self.assertRaisesRegex(ConfigError, "namespaced `a`"):
            Deployment(targets=[member("a"), member("a")]).to_config()

    def test_hosts_name_members(self) -> None:
        with self.assertRaisesRegex(ConfigError, "hosts names `c`"):
            Deployment(targets=[member("a"), member("b")], hosts={"c": "h"}).to_config()

    def test_two_bare_targets_raise(self) -> None:
        member("a")
        member("b")
        with self.assertRaisesRegex(ConfigError, "found 2"):
            deploy_mod.to_config()

    def test_two_deployments_raise(self) -> None:
        Deployment(targets=[member("a")])
        Deployment(targets=[member("b")])
        with self.assertRaisesRegex(ConfigError, "at most one Deployment"):
            deploy_mod.to_config()

    def test_deployment_must_include_every_target(self) -> None:
        member("a")
        Deployment(targets=[member("b")])
        with self.assertRaisesRegex(ConfigError, "leaves out 1 target"):
            deploy_mod.to_config()


class DeploymentTest(ConfigTest):
    def test_example_matches_golden(self) -> None:
        with contextlib.chdir(CRATE):
            design_example()
        written = json.dumps(deploy_mod.to_config(), indent=2) + "\n"
        with open(GOLDEN, encoding="utf-8") as file:
            self.assertEqual(written, file.read())

    def test_peer_transport_and_copies(self) -> None:
        config = design_example().to_config()
        mirror = config["targets"][1]["coordinator"]["systems"][0]
        self.assertEqual(
            mirror["params"]["transport"], {"peer": {"target": "a", "link": "pub"}}
        )
        self.assertEqual(
            mirror["outputs"],
            [
                {"port": "plant.imu", "peer": {"target": "a", "link": "pub", "port": "plant.imu"}},
                {"port": "plant.log", "peer": {"target": "a", "link": "pub", "port": "plant.log"}},
                {
                    "port": "plant.status",
                    "peer": {"target": "a", "link": "pub", "port": "plant.status"},
                },
                {"port": "nav.est", "peer": {"target": "a", "link": "pub", "port": "nav.est"}},
            ],
        )
        ctrl = config["targets"][1]["coordinator"]["systems"][2]
        self.assertEqual(ctrl["inputs"][0]["from"], [{"system": "mirror", "port": "nav.est"}])

    def test_no_items_copies_everything(self) -> None:
        a, plant, b = plant_and_peer()
        a_pub = a.add("pub", Publish([plant.imu, plant.log], transport=Listen("0.0.0.0:2240")))
        b.add("mirror", Subscribe(transport=Peer(a_pub)))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual([o["port"] for o in outputs], ["plant.imu", "plant.log"])

    def test_records_alone_copy_nothing(self) -> None:
        a, plant, b = plant_and_peer()
        a_pub = a.add("pub", Publish([plant], transport=Listen("0.0.0.0:2240")))
        b.add("cmds", Subscribe(transport=Peer(a_pub), records=[Ping]))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual(outputs, [{"port": "ping", "record": "ping"}])

    def test_port_returns_copy(self) -> None:
        a, plant, b = plant_and_peer()
        a_pub = a.add("pub", Publish([plant], transport=Listen("0.0.0.0:2240")))
        mirror = b.add("mirror", Subscribe(transport=Peer(a_pub), items=[plant]))
        imu = mirror.port(plant.imu)
        self.assertEqual((imu.ref.system, imu.ref.port, imu.record, imu.target), ("mirror", "plant.imu", Imu, b))
        log = mirror.port(plant.log)
        self.assertEqual((log.ref.port, log.record), ("plant.log", LogEvent))
        other = b.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "does not copy `plant.imu`"):
            mirror.port(other.imu)

    def test_builtin_log_finds_the_pack_copy(self) -> None:
        a, plant, b = plant_and_peer()
        cmds = a.add("cmds", Subscribe(transport=Listen("0.0.0.0:2241")))
        a_pub = a.add("pub", Publish([plant, cmds], transport=Listen("0.0.0.0:2240")))
        mirror = b.add("mirror", Subscribe(transport=Peer(a_pub)))
        self.assertIsNot(cmds.log.record, plant.log.record)
        self.assertEqual(mirror.port(cmds.log).ref.port, "plant.log")

    def test_foreign_source_raises(self) -> None:
        a, plant, b = plant_and_peer()
        b.add("nav", Nav(imu=plant.imu))
        with self.assertRaisesRegex(ConfigError, "`nav.imu` reads `plant.imu` on another"):
            Deployment(targets=[a, b]).to_config()

    def test_foreign_loop_producer_raises(self) -> None:
        a, plant, b = plant_and_peer()
        loop = b.loop(Imu)
        b.add("nav", Nav(imu=loop))
        loop.connect(plant.imu)
        with self.assertRaisesRegex(ConfigError, "on another target"):
            Deployment(targets=[a, b]).to_config()

    def test_port_on_plain_system_raises(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "copies no ports"):
            plant.port(plant.imu)

    def test_items_need_peer(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "needs a `Peer` transport"):
            Subscribe(items=[plant], transport=Listen("0.0.0.0:1"))

    def test_unpublished_item_reaches_the_config(self) -> None:
        a, plant, b = plant_and_peer()
        nav = a.add("nav", Nav(imu=plant.imu))
        a_pub = a.add("pub", Publish([plant.imu], transport=Listen("0.0.0.0:2240")))
        b.add("mirror", Subscribe(transport=Peer(a_pub), items=[nav.est]))
        mirror = Deployment(targets=[a, b]).to_config()["targets"][1]["coordinator"]["systems"][0]
        peer = {"target": "a", "link": "pub", "port": "nav.est"}
        self.assertEqual(mirror["outputs"][0], {"port": "nav.est", "peer": peer})

    def test_peer_takes_a_handle(self) -> None:
        with self.assertRaisesRegex(ConfigError, "takes a link's handle"):
            Peer("pub")  # type: ignore[arg-type]

    def test_peer_target_needs_namespace(self) -> None:
        a = Target(cycle_rate=100.0)
        a_pub = a.add("pub", Publish(transport=Listen("0.0.0.0:2240")))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("mirror", Subscribe(transport=Peer(a_pub)))
        with self.assertRaisesRegex(ConfigError, "names no namespace"):
            b.to_config()

    def test_db_emits_builtin_with_all(self) -> None:
        gw = Target(cycle_rate=10.0, namespace="gw")
        plant = gw.add("plant", Plant())
        gw.add("db", Db(listen="0.0.0.0:2250", all=True), thread="db")
        entry = gw.to_config()["coordinator"]["systems"][1]
        self.assertEqual(entry["ty"], "fsw.db")
        self.assertEqual(entry["thread"], "db")
        self.assertEqual(entry["params"]["command_ids"], [])
        self.assertIsNone(entry["params"]["path"])
        self.assertEqual([i["port"] for i in entry["inputs"]], ["plant.imu", "plant.log", "plant.status"])
        self.assertEqual(plant.imu.record, Imu)

    def test_gateway_composes_subscribes_and_db(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        a_pub = a.add("pub", Publish([plant], transport=Listen("0.0.0.0:2240")))
        gw, db = gateway("gw", peers=[a_pub], listen="0.0.0.0:2250")
        config = Deployment(targets=[a, gw]).to_config()
        systems = config["targets"][1]["coordinator"]["systems"]
        self.assertEqual([s["id"] for s in systems], ["a", "db"])
        self.assertEqual(
            [i["port"] for i in systems[1]["inputs"]],
            ["a.plant.imu", "a.plant.log", "a.plant.status", "a.link_status", "a.log", "a.status"],
        )
        self.assertEqual(db.db_status.record.__name__, "DbStatus")

    def test_subscribe_copies_are_publishable(self) -> None:
        a, plant, b = plant_and_peer()
        a_pub = a.add("pub", Publish([plant.imu], transport=Listen("0.0.0.0:2240")))
        mirror = b.add("mirror", Subscribe(transport=Peer(a_pub)))
        b.add("pub", Publish([mirror], transport=Listen("0.0.0.0:2241")))
        config = Deployment(targets=[a, b]).to_config()
        pub = config["targets"][1]["coordinator"]["systems"][1]
        self.assertEqual(
            [i["port"] for i in pub["inputs"]],
            ["mirror.plant.imu", "mirror.link_status", "mirror.log", "mirror.status"],
        )

    def test_message_arrives_on_one_port(self) -> None:
        a, plant, b = plant_and_peer()
        nav = a.add("nav", Nav(imu=plant.imu))
        a_pub = a.add("pub", Publish([plant, nav], transport=Listen("0.0.0.0:2240")))
        mirror = b.add("mirror", Subscribe(transport=Peer(a_pub)))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual(
            [o["port"] for o in outputs],
            ["plant.imu", "plant.log", "plant.status", "nav.est", "nav.status"],
        )
        self.assertEqual(mirror.port(nav.log).ref.port, "plant.log")
        self.assertEqual(mirror.port(nav.status).ref.port, "nav.status")
