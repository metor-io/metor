# Deployments

At their core a deployment is a set of targets that can talk to each other, share ICDs between each other, and can be deployed and run together.  Roughly there are three major features that make up a deployment: running / deploying, peer to peer communication, shared config, and a unified "gateway".

## Shared Configuration

Right now each target has a single target.py file. It defines which packs to build, what systems to run, and the connections between them. 

A deployment is very similar, but it is made up of a number of targets. For instnace you could define the following in rough pseudocode:
```python
plant = Target(namespace="plant")
fsw = Target(namespace="fsw")

plant.add("plant", Plant())
fsw.add("fsw", Fsw())

deploy = Deployment(targets = [plant, fsw])
```

This gives you a single configuration file that metor can run. To run your deploy a single target you can access it from the larger deployment config like:
```
metor-fsw run deployment --target fsw
metor-fsw run deployment --target plant
```

### Running / Deploying

For local development you can run the entire deployment with the following
```
metor-fsw run deployment
```

This will launch all the targets as separate processes, and prefix their log outputs with their namespace name. This will use the same tasteful styling as the rest of the CLI.

For deploying, we will have a command that renders a target config into a series of NixOS configurations, that can either be deployed onto real machines, or a series of microVMs. This a future TBD feature, but should be considered in the first impl

### Cross target coms

Having multiple targets is only useful if you have the ability to communicate between them. Ideally this looks very similar to how we connnect systems together, but with some minor differences.

In pseduo-code we could define connecitons like this:
```python
fsw_publish = fsw.add("publish", Publish())

fsw.connect(fsw.torque_cmd, publish.torque_cmd)
fsw.connect(fsw.mtq_cmd, publish.mtq_cmd)

subscribe = fsw.add("subscribe", Subscribe(to = plant_publish))

fsw.connect(subscribe.gps, fsw.gps)
```

You can see in the above there is some level of type interference going on. Since this is showing cross-target coms, there should be some element of strong typing. I am subscribing to outputs from the plant; I should only be able to do that if the types match.

There are a lot of TBDs in here:
- How do we achieve that level of strong typing, while being extremely ergonomic?
- How do we do service discover and IP allocation? By putting them all in the same config here, we have an oppertunity to make this very easy. But how do we achieve it?

To achieve this we should reuse as much of the existing telemetry system as possible. There are some obvious differences here, but fundementally the idea of sending components over the wire is the same.

### Gateway

A gateway is a new metor-fsw instance with metor-db embedded that acts as a bridge between all the targets and metor-panel or any other ground-system. All targets should publish telemetry to it, and then metor-panel can connect to it.

Ideally that connection used the pre-exisitng DB to DB sync that we have created
