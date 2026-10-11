"""Tests for deployments: peers, copied ports, and the golden config file."""

from __future__ import annotations

import json
import os

from adcs_stub import Ctrl, Imu, LogEvent, Mode, Nav, Ping, Plant
from test_target import ConfigTest, adcs_target, reset

import metor_config._deployment as deploy_mod
from metor_config import ConfigError, Deployment, Publish, Subscribe, Target

GOLDEN = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "tests", "golden", "deployment.json"
)


def design_example() -> Deployment:
    """A two-target file, over the stub pack."""
    a = Target(cycle_rate=100.0, namespace="a")
    plant = a.add("plant", Plant())
    nav = a.add("nav", Nav(imu=plant.imu))
    a_pub = a.add("pub", Publish(listen="0.0.0.0:2240", all=True))

    b = Target(cycle_rate=100.0, namespace="b")
    mirror = b.add("mirror", Subscribe(peer=a_pub, items=[plant, nav.est]))
    mode = b.add("mode", Mode())
    b.add("ctrl", Ctrl(est=mirror.port(nav.est), mode=mode.cmd))
    b_pub = b.add("pub", Publish(listen="0.0.0.0:2241", all=True))

    a.add("cmds", Subscribe(peer=b_pub, records=[Ping]))
    return Deployment(targets=[a, b], hosts={"a": "10.0.0.2"})


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
        a_pub = a.add("pub", Publish([plant.imu, plant.log], listen="0.0.0.0:2240"))
        b.add("mirror", Subscribe(peer=a_pub))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual([o["port"] for o in outputs], ["plant.imu", "plant.log"])

    def test_records_alone_copy_nothing(self) -> None:
        a, plant, b = plant_and_peer()
        a_pub = a.add("pub", Publish([plant], listen="0.0.0.0:2240"))
        b.add("cmds", Subscribe(peer=a_pub, records=[Ping]))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual(outputs, [{"port": "ping", "record": "ping"}])

    def test_port_returns_copy(self) -> None:
        a, plant, b = plant_and_peer()
        a_pub = a.add("pub", Publish([plant], listen="0.0.0.0:2240"))
        mirror = b.add("mirror", Subscribe(peer=a_pub, items=[plant]))
        imu = mirror.port(plant.imu)
        self.assertEqual((imu.ref.system, imu.ref.port, imu.record, imu.target), ("mirror", "plant.imu", Imu, b))
        log = mirror.port(plant.log)
        self.assertEqual((log.ref.port, log.record), ("plant.log", LogEvent))
        other = b.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "does not copy `plant.imu`"):
            mirror.port(other.imu)

    def test_builtin_log_finds_the_pack_copy(self) -> None:
        a, plant, b = plant_and_peer()
        cmds = a.add("cmds", Subscribe(listen="0.0.0.0:2241"))
        a_pub = a.add("pub", Publish([plant, cmds], listen="0.0.0.0:2240"))
        mirror = b.add("mirror", Subscribe(peer=a_pub))
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
        with self.assertRaisesRegex(ConfigError, "needs `peer`"):
            Subscribe(items=[plant], listen="0.0.0.0:1")

    def test_unpublished_item_raises(self) -> None:
        a, plant, b = plant_and_peer()
        nav = a.add("nav", Nav(imu=plant.imu))
        a_pub = a.add("pub", Publish([plant.imu], listen="0.0.0.0:2240"))
        b.add("mirror", Subscribe(peer=a_pub, items=[nav.est]))
        with self.assertRaisesRegex(ConfigError, "does not publish `nav.est`"):
            Deployment(targets=[a, b]).to_config()

    def test_peer_must_be_a_link(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "takes a link's handle"):
            Subscribe(peer=plant)  # pyright: ignore[reportArgumentType]

    def test_peer_must_be_a_member(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        a_pub = a.add("pub", Publish(listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("mirror", Subscribe(peer=a_pub))
        with self.assertRaisesRegex(ConfigError, "does not include"):
            Deployment(targets=[b]).to_config()

    def test_peer_must_listen_on_a_fixed_port(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        dialer = a.add("out", Publish(connect="127.0.0.1:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("mirror", Subscribe(peer=dialer))
        with self.assertRaisesRegex(ConfigError, "which dials"):
            Deployment(targets=[a, b]).to_config()
        reset()
        a = Target(cycle_rate=100.0, namespace="a")
        any_port = a.add("pub", Publish(listen="127.0.0.1:0"))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("mirror", Subscribe(peer=any_port))
        with self.assertRaisesRegex(ConfigError, "takes any port"):
            Deployment(targets=[a, b]).to_config()

    def test_peer_target_needs_namespace(self) -> None:
        a = Target(cycle_rate=100.0)
        a_pub = a.add("pub", Publish(listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("mirror", Subscribe(peer=a_pub))
        with self.assertRaisesRegex(ConfigError, "names no namespace"):
            b.to_config()

    def test_one_transport_only(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        a_pub = a.add("pub", Publish(listen="0.0.0.0:2240"))
        with self.assertRaisesRegex(ConfigError, "exactly one"):
            Subscribe(peer=a_pub, connect="127.0.0.1:1")
        with self.assertRaisesRegex(ConfigError, "no `max_connections`"):
            Subscribe(peer=a_pub, max_connections=2)

    def test_host_port_collision(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        a.add("pub", Publish(listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("pub", Publish(listen="[::]:2240"))
        with self.assertRaisesRegex(ConfigError, "both bind 127.0.0.1:2240"):
            Deployment(targets=[a, b]).to_config()
        Deployment(targets=[a, b], hosts={"b": "10.0.0.2"}).to_config()

    def test_subscribe_copies_are_publishable(self) -> None:
        a, plant, b = plant_and_peer()
        a_pub = a.add("pub", Publish([plant.imu], listen="0.0.0.0:2240"))
        mirror = b.add("mirror", Subscribe(peer=a_pub))
        b.add("pub", Publish([mirror], listen="0.0.0.0:2241"))
        config = Deployment(targets=[a, b]).to_config()
        pub = config["targets"][1]["coordinator"]["systems"][1]
        self.assertEqual(
            [i["port"] for i in pub["inputs"]],
            ["mirror.plant.imu", "mirror.link_status", "mirror.log", "mirror.status"],
        )

    def test_message_arrives_on_one_port(self) -> None:
        a, plant, b = plant_and_peer()
        nav = a.add("nav", Nav(imu=plant.imu))
        a_pub = a.add("pub", Publish([plant, nav], listen="0.0.0.0:2240"))
        mirror = b.add("mirror", Subscribe(peer=a_pub))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual(
            [o["port"] for o in outputs],
            ["plant.imu", "plant.log", "plant.status", "nav.est", "nav.status"],
        )
        self.assertEqual(mirror.port(nav.log).ref.port, "plant.log")
        self.assertEqual(mirror.port(nav.status).ref.port, "nav.status")
