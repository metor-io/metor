"""Deployments: peers, copied ports, the gateway, and the envelope golden."""

from __future__ import annotations

import json
import os
import unittest

from adcs_stub import Ctrl, Est, Imu, LogEvent, Mode, Nav, Ping, Plant
from test_target import reset

from metor_config import ConfigError, Db, Deployment, Publish, Subscribe, Target, gateway
from metor_config._deployment import to_config

GOLDEN = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", "tests", "golden", "deployment.json"
)


def design_example() -> Deployment:
    """The design doc's three-target file, over the stub pack."""
    a = Target(cycle_rate=100.0, namespace="a")
    plant = a.add("plant", Plant())
    nav = a.add("nav", Nav(imu=plant.imu))
    a_pub = a.add("pub", Publish(listen="0.0.0.0:2240", all=True))

    b = Target(cycle_rate=100.0, namespace="b")
    mirror = b.add("mirror", Subscribe(peer=a_pub, items=[plant, nav.est]))
    mode = b.add("mode", Mode())
    b.add("ctrl", Ctrl(est=mirror.port(nav.est), mode=mode.cmd))
    b_pub = b.add("pub", Publish(listen="0.0.0.0:2241", all=True))

    gw, gw_db = gateway("gw", peers=[a_pub, b_pub], listen="0.0.0.0:2250")
    a.add("cmds", Subscribe(peer=gw_db, records=[Ping]))
    return Deployment(targets=[a, b, gw], hosts={"a": "10.0.0.2"})


class DeploymentTest(unittest.TestCase):
    def setUp(self) -> None:
        reset()
        os.environ.pop("METOR_FSW_ABI_VERSION", None)

    def tearDown(self) -> None:
        reset()

    def test_example_matches_golden(self) -> None:
        design_example()
        written = json.dumps(to_config(), indent=2) + "\n"
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
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        a_pub = a.add("pub", Publish([plant.imu, plant.log], listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("mirror", Subscribe(peer=a_pub))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual([o["port"] for o in outputs], ["plant.imu", "plant.log"])

    def test_records_alone_copy_nothing(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        a_pub = a.add("pub", Publish([plant], listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        b.add("cmds", Subscribe(peer=a_pub, records=[Ping]))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual(outputs, [{"port": "ping", "record": "ping"}])

    def test_port_returns_copy(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        a_pub = a.add("pub", Publish([plant], listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        mirror = b.add("mirror", Subscribe(peer=a_pub, items=[plant]))
        imu = mirror.port(plant.imu)
        self.assertEqual((imu.ref.system, imu.ref.port, imu.record, imu.target), ("mirror", "plant.imu", Imu, b))
        log = mirror.port(plant.log)
        self.assertEqual((log.ref.port, log.record), ("plant.log", LogEvent))
        other = b.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "does not copy `plant.imu`"):
            mirror.port(other.imu)

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
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        nav = a.add("nav", Nav(imu=plant.imu))
        a_pub = a.add("pub", Publish([plant.imu], listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
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
        a_pub = a.add("pub", Publish([plant], listen="0.0.0.0:2240"))
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
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        a_pub = a.add("pub", Publish([plant.imu], listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        mirror = b.add("mirror", Subscribe(peer=a_pub))
        b.add("pub", Publish([mirror], listen="0.0.0.0:2241"))
        config = Deployment(targets=[a, b]).to_config()
        pub = config["targets"][1]["coordinator"]["systems"][1]
        self.assertEqual(
            [i["port"] for i in pub["inputs"]],
            ["mirror.plant.imu", "mirror.link_status", "mirror.log", "mirror.status"],
        )

    def test_est_record_is_a_frame(self) -> None:
        self.assertEqual(Est._kind, "frame")

    def test_message_arrives_on_one_port(self) -> None:
        a = Target(cycle_rate=100.0, namespace="a")
        plant = a.add("plant", Plant())
        nav = a.add("nav", Nav(imu=plant.imu))
        a_pub = a.add("pub", Publish([plant, nav], listen="0.0.0.0:2240"))
        b = Target(cycle_rate=100.0, namespace="b")
        mirror = b.add("mirror", Subscribe(peer=a_pub))
        config = Deployment(targets=[a, b]).to_config()
        outputs = config["targets"][1]["coordinator"]["systems"][0]["outputs"]
        self.assertEqual(
            [o["port"] for o in outputs],
            ["plant.imu", "plant.log", "plant.status", "nav.est", "nav.status"],
        )
        self.assertEqual(mirror.port(nav.log).ref.port, "plant.log")
        self.assertEqual(mirror.port(nav.status).ref.port, "nav.status")
