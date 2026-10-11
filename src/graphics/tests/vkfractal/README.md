# vkfractal

vkfractal is a Vulkan benchmark that renders a zoom into the Mandelbrot set. Each pixel runs up
to `--max-iterations` iterations in a fragment shader that reads no textures or buffers, so the
frame time depends on the GPU's shader ALU throughput rather than its memory bandwidth.

vkfractal runs as a system test. It takes over the display while it runs and returns it when it
exits. It prints how long the timed frames took, and fails if a Vulkan call fails or the
validation layers report an error.

## Running

```
fx test vkfractal
fx test vkfractal -- --frames=100 --validation=false
fx test vkfractal -- --resolution=1024x1024
fx test vkfractal -- --offscreen
```

The result line looks like this:

```
vkfractal: 600 frames (600 presented) in 12.000 s: 20.000 ms/frame, 50.0 fps
```

`presented` is the number of timed frames that reached the display (see
[Presentation](#presentation)).

## Options

| Option | Default | Description |
| --- | --- | --- |
| `--frames=N` | 600 | Frames to render and time. |
| `--warmup-frames=N` | 10 | Untimed frames rendered first, so that one-time driver work is not timed. |
| `--total-zoom=Z` | 4000 | Magnification of the last frame relative to the first. |
| `--max-iterations=N` | 1024 | Iteration cap per pixel. |
| `--frames-in-flight=N` | 3 | Frames kept queued on the GPU, at most 16. |
| `--validation=BOOL` | true | Enables the Vulkan validation layers. |
| `--offscreen` | false | Renders without the display. |
| `--resolution=WxH` | The display's size | Points iterated per frame. |
| `--extents=WxH` | 3.5x2, fitted to the display | Frame 0's width and height in the complex plane. |
| `--center=RE,IM` | A point in seahorse valley | The point every frame is centered on. |

`fx test vkfractal -- --help` prints the same list.

## The zoom

Frame 0 covers `--extents` of the complex plane, centered on `--center`. Every later frame
magnifies by the same ratio, so the last frame is magnified `--total-zoom` times relative to
frame 0. The default extents are 3.5x2, the size of the classic view of the whole set, shrunk
along one axis to the display's aspect ratio (for example, 3.5x1.96875 on a 1920x1080 display).
The default center is a point in seahorse valley that shows detail at every zoom level.
vkfractal prints the extents and center in use before it starts.

## Comparing devices

The default resolution is the display's size, so the default workload differs between devices.
To run the same workload on different devices, pass `--resolution` and `--extents` (and
`--center`, if not the default) explicitly. The whole image is always stretched to fill the
display, so extents whose aspect ratio differs from the display's look squished, but the workload
is the same.

The display swapchain shows images at their own size. When the resolution differs from the
display's, each frame renders into an offscreen image, and a bilinear pass (`shaders/scale.frag`)
stretches it over the display. On a display that is taller than it is wide, the image is also
rotated 90 degrees clockwise, and the default resolution is the display's size swapped (for
example, 1920x1080 on a 1080x1920 display).

With `--offscreen`, vkfractal creates no surface or swapchain and renders every frame into an
image that is never shown, 1024x1024 by default. The default extents then follow that image's
aspect ratio.

## Presentation

vkfractal never waits for the display. It keeps `--frames-in-flight` frames queued on the GPU,
and before recording a frame it waits only for the frame that last used the same slot. Each frame
takes a swapchain image only if one is already free. If it gets one, the frame is drawn into it,
directly or through the bilinear pass, and presented right away. Otherwise the frame renders into
an offscreen image and is discarded. At each vsync, the display shows the newest frame that has
finished rendering and returns the older ones. The swapchain has `--frames-in-flight` + 2 images,
at most 10, so when rendering is slower than the display, every frame is shown.

## Shaders

The GLSL shaders in `shaders/` are compiled to SPIR-V headers at build time by `BUILD.gn`.
