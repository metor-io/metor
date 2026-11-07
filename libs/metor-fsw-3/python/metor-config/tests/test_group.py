"""Tests for groups."""

from __future__ import annotations

from typing import NamedTuple

from adcs_stub import Est, Hold, Imu, Nav, Plant
from test_target import ConfigTest

import metor_config._deployment as deploy_mod
from metor_config import (
    ConfigError,
    Deployment,
    Group,
    Listen,
    Loop,
    OutPort,
    Peer,
    Publish,
    Subscribe,
    Target,
    group,
)


class Node(NamedTuple):
    target: Target
    link: Publish
    imu: OutPort[Imu]


@group
def node(g: Group, *, port: int) -> Node:
    t = g.target(cycle_rate=10.0)
    plant = t.add("plant", Plant())
    link = t.add("pub", Publish([plant], transport=Listen(f"127.0.0.1:{port}")))
    return Node(t, link, plant.imu)


@group
def pair(g: Group, *, base: int) -> tuple[Node, Node]:
    return g.add("a", node(port=base)), g.add("b", node(port=base + 1))


def namespaces(d: Deployment) -> list[str]:
    return [t["coordinator"]["namespace"] for t in d.to_config()["targets"]]


class NominalTest(ConfigTest):
    def test_tree_names_targets_by_path(self) -> None:
        d = Deployment()
        d.add("flight", pair(base=2240))
        d.add("ground", node(port=2250))
        self.assertEqual(namespaces(d), ["flight.a", "flight.b", "ground"])

    def test_returns_the_functions_result(self) -> None:
        d = Deployment()
        a, b = d.add("flight", pair(base=2240))
        self.assertEqual(a.target.namespace, "flight.a")
        self.assertEqual(b.imu.target, b.target)

    def test_group_added_twice_runs_twice(self) -> None:
        d = Deployment()
        build = node(port=2240)
        one, two = d.add("one", build), d.add("two", build)
        self.assertIsNot(one.target, two.target)
        self.assertEqual(namespaces(d), ["one", "two"])

    def test_named_target(self) -> None:
        @group
        def split(g: Group) -> None:
            g.target("io", cycle_rate=10.0)
            g.target("fsw", cycle_rate=20.0)

        d = Deployment()
        d.add("cpu", split())
        self.assertEqual(namespaces(d), ["cpu.io", "cpu.fsw"])

    def test_parent_closes_a_cycle(self) -> None:
        class Gateway(NamedTuple):
            target: Target
            link: Publish
            est: Loop[Est]

        @group
        def gateway(g: Group) -> Gateway:
            t = g.target(cycle_rate=10.0)
            est = g.loop(Est)
            plant = t.add("plant", Plant())
            t.add("check", Hold(est=est))
            link = t.add("pub", Publish([plant], transport=Listen("127.0.0.1:2240")))
            return Gateway(t, link, est)

        @group
        def cpu(g: Group, *, gateway: Publish, imu: OutPort[Imu]) -> Node:
            t = g.target(cycle_rate=10.0)
            up = t.add("uplink", Subscribe(transport=Peer(gateway), items=[imu]))
            nav = t.add("nav", Nav(imu=up.port(imu)))
            link = t.add("pub", Publish([nav], transport=Listen("127.0.0.1:2241")))
            return Node(t, link, up.port(imu))

        @group
        def string(g: Group) -> None:
            gw = g.add("gw", gateway())
            plant_imu = gw.target._handles["plant"].imu
            c = g.add("cpu", cpu(gateway=gw.link, imu=plant_imu))
            nav = c.target._handles["nav"]
            down = gw.target.add("down", Subscribe(transport=Peer(c.link), items=[nav.est]))
            gw.est.connect(down.port(nav.est))

        d = Deployment()
        d.add("s1", string())
        targets = {t["coordinator"]["namespace"]: t for t in d.to_config()["targets"]}
        gw_steps = {s["id"]: s for s in targets["s1.gw"]["coordinator"]["systems"]}
        est = next(i for i in gw_steps["check"]["inputs"] if i["port"] == "est")
        self.assertEqual(est["from"], [{"system": "down", "port": "nav.est"}])
        resources = targets["s1.gw"]["coordinator"]["resources"]
        self.assertEqual(resources["down_ep"], {"peer": {"target": "s1.cpu", "link": "pub"}})


class HostsTest(ConfigTest):
    def test_longest_key_wins(self) -> None:
        d = Deployment(hosts={"flight": "10.0.0.1", "flight.b": "10.0.0.2"})
        d.add("flight", pair(base=2240))
        d.add("ground", node(port=2250))
        hosts = d.to_config()["hosts"]
        self.assertEqual(hosts, {"flight.a": "10.0.0.1", "flight.b": "10.0.0.2"})

    def test_group_and_plain_targets_mix(self) -> None:
        gse = Target(cycle_rate=10.0, namespace="gse")
        d = Deployment(targets=[gse], hosts={"gse": "10.0.1.1"})
        d.add("flight", pair(base=2240))
        self.assertEqual(namespaces(d), ["gse", "flight.a", "flight.b"])
        self.assertEqual(d.to_config()["hosts"], {"gse": "10.0.1.1"})
        self.assertEqual(deploy_mod.to_config(), d.to_config())

    def test_key_must_match_a_dot_boundary(self) -> None:
        d = Deployment(hosts={"fli": "10.0.0.1"})
        d.add("flight", pair(base=2240))
        with self.assertRaisesRegex(ConfigError, "hosts names `fli`"):
            d.to_config()


class FailureTest(ConfigTest):
    def test_bad_names(self) -> None:
        d = Deployment()
        for name in ("", "a.b", "a b", "1a"):
            with self.assertRaisesRegex(ConfigError, "is an identifier"):
                d.add(name, node(port=2240))

        @group
        def bad(g: Group) -> None:
            g.target("a-b", cycle_rate=10.0)

        with self.assertRaisesRegex(ConfigError, "is an identifier, got 'a-b'"):
            d.add("x", bad())

    def test_path_taken(self) -> None:
        d = Deployment()
        d.add("flight", pair(base=2240))
        with self.assertRaisesRegex(ConfigError, "group `flight` is already added"):
            d.add("flight", pair(base=2250))

    def test_target_and_child_share_a_path(self) -> None:
        @group
        def both(g: Group) -> None:
            g.target(cycle_rate=10.0)
            g.add("inner", node(port=2240))

        d = Deployment()
        d.add("cpu", both())
        with self.assertRaisesRegex(ConfigError, "`cpu.inner` sits under target `cpu`"):
            d.to_config()

    def test_target_takes_no_namespace(self) -> None:
        @group
        def named(g: Group) -> None:
            g.target(cycle_rate=10.0, namespace="x")  # pyright: ignore[reportCallIssue]

        with self.assertRaises(TypeError):
            Deployment().add("x", named())

    def test_not_a_group(self) -> None:
        with self.assertRaisesRegex(ConfigError, "`x` takes a group"):
            Deployment().add("x", Target(10.0))  # pyright: ignore[reportArgumentType]

    def test_arguments_fail_at_the_call(self) -> None:
        with self.assertRaises(TypeError):
            node()  # pyright: ignore[reportCallIssue]
        with self.assertRaises(TypeError):
            node(port=1, extra=2)  # pyright: ignore[reportCallIssue]

    def test_target_left_out(self) -> None:
        Target(10.0, namespace="stray")
        d = Deployment()
        d.add("flight", pair(base=2240))
        with self.assertRaisesRegex(ConfigError, "leaves out 1 target"):
            deploy_mod.to_config()
