import handler from "vinext/server/fetch-handler";

const worker = {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);
    if (url.hostname === "www.meshrmm.com") {
      url.hostname = "meshrmm.com";
      return Response.redirect(url, 308);
    }

    return handler.fetch(request, env, ctx);
  },
} satisfies ExportedHandler<Env>;

export default worker;
