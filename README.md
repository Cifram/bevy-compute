This is a plugin for the Bevy game engine to simplify use of compute shaders.

To initiate the compute shaders, first set up all the needed buffers in the `ShaderBufferSet`. Then, send a `StartComputeEvent`, which will contain all the info on the compute shader passes and pipelines, and be prepared to recieve `CopyBufferEvent`, which will have buffer data returned from the computer shaders back to the CPU, and `ComputeGroupDoneEvent`, which will tell you that a given compute pipeline group has completed.