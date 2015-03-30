# Snowstorm

`Snowstorm` is a purposed work queue system to allow for parallelism of systems inside of a game engine. It is based on my previous work in `snowmew` and realizing some of the major limitations of its ECS.

The fundamental problem with `snowmew` ended up being that the `world` state ended up being very difficult to compose. Each new component-system would make the `world` bigger. There was no obvious way to decouple the system either since `snowmew` wanted a large snapshot view of the world. This has some nice advantages, like being able to reverse time. But ultimately I felt the architecture was unwieldy as projects grew more complicate.

The other problem was that `snowmew` had trouble reusing components from other projects. `nphysics` for example could not be used since it managed its own state internally. It would be more work then it was worth to modify the library to support `COW` snapshotting.

A related problem was the `view` problem. Snowmew was designed out the idea of tables of data that were fast to iterate over and cheap to modify without bulk reallocation (`via COW`). The problem is that this was the _only_ way to access upstream data. The render would have run over every texture, material, geometry to find out what needed to be downloaded to the gpu. Since most of these are created once during initialization, this is completely wasted effort. Secondly, a scene manager will probably want to store the scene (or parts of it) into a BVH tree to make it fast to cull. Rebuilding the tree each frame is expensive.

## Concepts

The work required to update a frame can be broken down into a number of smaller system. Each system is responsible for working on one type of data and producing an updated state from that data. A system itself can be broken down further into smaller tasks, but this will be opaque to the rest of the system.

Systems communicate via Multiple-Sender Multiple-Consumer channels, a message that is sent from one sender will be read by all consumers. In addition, channels communicate what their current frame number is. This is used as a virtual `clock` to keep all the in sync and make sure that messages sent on the channels are only updated for the downstream components in sync with the current frame updates.

![Example Pipeline](http://i.imgur.com/HyPuaOj.png)

## A System

A System has two phases of execution. The `gather` phase is the phase when the system is collecting messages from upstream components. A system may run when any upstream component has produced data to be consumed, as long as the data is for the current clock. The `update` phase occurs after all input channels transition to a the next clock phase. For a system, this means no new input will occur for this frame and we can work on the gartered data, or previous data to progress. After the `update` phase is complete the system clock is automatically updated, all channels are flushed and this system may start the `gather` phase again.

One of the neat side effects of this `gather` `update` design is that the `gather` for a system can happen ahead of time. Meaning that work for frame `N+1` can start during the idle periods of frame `N`. This is not a problem since the output of a system is a serious of messages, and when those are produced does not matter.

## Subsystems

Since a system is a black-block that takes messages in, and sends out messages. It is also obvious that a system does not need to be monolithic, it can be made up of smaller components that work on a much smaller work load. 

Sharding can easily be done if the work is as simple as `f(x) -> x+1`. Meaning all the available cores could be loaded without much additional work by the developer.

![Sharded system](http://i.imgur.com/3LrUA9F.png)

## Backwards Communication

One problem with this design is that it is not possible to communicate backwards is impossible without a special primitive. If two systems depend on each other's output, or a system depends on its own output that channel can never `clock`. The reason being that a system is clocked when all channels are at the same phase. If a feedback loop exists, the channels phase will never advance since the system is waiting on the channel, and the channel is waiting on the system.

To get around this problem there is a delay block (or `z-1`), which is a stolen from DSP to represent a time delay. The idea is that this block is always one clock-phase ahead of its input channel. When it is clocked, it copies all the input that it gathers to the output channel.


# Building an `ECS` out of `Snowstorm`

`Snowstorm` answers the question of how systems should run. It does not answer how system should communicate and how they should work together.

Each message is either a Insert, or a Delete. The systems downstream can easily take these changes and build a mapping in what ever data structure they need. This is internal to the system, and does not need to be shared. So the system can write them out to OpenCL or into a fast hash-table. What ever it needs to get work done.

```rust
enum Change<K, V> {
    Insert(K, V),
    Delete(K)
}
```

The results of each system would be another set of Key-Value pairs that are sent to other subsystems.

## Problems

There are two problems with breaking up the system state fine. The big one is that in the fan-out case multiple writes can occur to the same key in a single frame update. With this design who ever wrote last would end up winning with their value. This is almost always going to be a bug, so it should be possible to inject systems at compile time that catch these exceptional states. These could be removed with the `ndebug` flag later.

The other problem is in how to create keys. This is required if the system is able to create new entities in the game. Of course not all systems need this ability.

One solution is pass around a `token` that is used by one system at a time to create new entities. This would probably require some type of `SPSC` channel be added to `snowstorm`. This token can the be passed used to create new entities via something as simple as bumping an integer. This could be an atomic integer, which would mean this could be used in the `fan-out` case. There would need to be some way to reuse keys which would be easy in the none-fanout case, but require mutexs otherwise.

Another solution is to use a library like [snowflake](https://github.com/Stebalien/snowflake) to create unique keys for us. This would probably be pretty back for keeping keys packed into a small space, but as long as everyone was aware that keys were generated in this fashion, they could find ways around it.

## Neat Side Effects

Background saving of the game should not only be possible, but easy. You just needs a system that listens for `Change` messages from relevant subsystems. Compacts them into a Map, then writes that map out to disk every few frames. Loading this save would be a matter of reading the saved data from disk, and converting it into a series of inserts on the relevant channels.
