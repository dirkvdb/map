// Render through an 8-bit canvas: cargo-bundle cannot encode RGBA16 PNGs as ICNS.
for (const size of [512, 1024]) {
    const canvas = new Canvas(size, size);
    const context = canvas.getContext("2d");
    context.drawImage(sips.images[0], 0, 0, size, size);
    const name = size === 512 ? "icon_512x512.png" : "icon_512x512@2x.png";
    new Output(context, name, "public.png").addToQueue();
}
