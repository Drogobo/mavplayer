//! libavfilter graph construction shared by the audio and video paths.

use super::av::{self, FilterGraph, Frame, ff};
use std::os::raw::c_int;
use std::ptr;

pub struct Graph {
    /// Owns the graph the two filter contexts live in.
    _graph: FilterGraph,
    pub src: *mut ff::AVFilterContext,
    pub sink: *mut ff::AVFilterContext,
}
unsafe impl Send for Graph {}

impl Graph {
    /// Builds `src -> chain -> sink`. `chain` is a libavfilter description
    /// such as `"eq=contrast=1.2,hflip"`; empty means pass-through.
    pub fn new(
        src_filter: &str,
        src_args: &str,
        sink_filter: &str,
        chain: &str,
        threads: i32,
    ) -> anyhow::Result<Graph> {
        Self::with_setup(src_filter, src_args, sink_filter, chain, threads, &mut |_| Ok(()))
    }

    /// Like [`Graph::new`], calling `setup` on each filter of `chain` after
    /// it is created but before its options are applied and it initialises
    /// (to give it a hardware device, or binary options).
    pub fn with_setup(
        src_filter: &str,
        src_args: &str,
        sink_filter: &str,
        chain: &str,
        threads: i32,
        setup: &mut dyn FnMut(*mut ff::AVFilterContext) -> anyhow::Result<()>,
    ) -> anyhow::Result<Graph> {
        unsafe {
            let graph = FilterGraph(ff::avfilter_graph_alloc());
            if graph.0.is_null() {
                anyhow::bail!("avfilter_graph_alloc failed");
            }
            (*graph.0).nb_threads = threads;
            let (sf, kf) = (av::cstr(src_filter), av::cstr(sink_filter));
            let (in_name, out_name) = (av::cstr("in"), av::cstr("out"));
            let args = av::cstr(src_args);
            let mut src = ptr::null_mut();
            av::check(
                ff::avfilter_graph_create_filter(
                    &mut src,
                    ff::avfilter_get_by_name(sf.as_ptr()),
                    in_name.as_ptr(),
                    args.as_ptr(),
                    ptr::null_mut(),
                    graph.0,
                ),
                "buffer source",
            )?;
            let mut sink = ptr::null_mut();
            av::check(
                ff::avfilter_graph_create_filter(
                    &mut sink,
                    ff::avfilter_get_by_name(kf.as_ptr()),
                    out_name.as_ptr(),
                    ptr::null(),
                    ptr::null_mut(),
                    graph.0,
                ),
                "buffer sink",
            )?;
            let chain =
                if chain.trim().is_empty() { if src_filter == "abuffer" { "anull" } else { "null" } } else { chain };
            let desc = av::cstr(chain);
            // What avfilter_graph_parse_ptr does, with `setup` in between:
            // parse, create the filters, set them up, initialise and link
            // them, then join the chain's open ends to the source and sink.
            let mut seg: *mut ff::AVFilterGraphSegment = ptr::null_mut();
            let mut inputs: *mut ff::AVFilterInOut = ptr::null_mut();
            let mut outputs: *mut ff::AVFilterInOut = ptr::null_mut();
            let r = (|| {
                av::check(ff::avfilter_graph_segment_parse(graph.0, desc.as_ptr(), 0, &mut seg), "parse")?;
                av::check(ff::avfilter_graph_segment_create_filters(seg, 0), "create filters")?;
                for c in 0..(*seg).nb_chains {
                    let ch = *(*seg).chains.add(c);
                    for i in 0..(*ch).nb_filters {
                        let p = *(*ch).filters.add(i);
                        if !(*p).filter.is_null() {
                            setup((*p).filter)?;
                        }
                    }
                }
                av::check(ff::avfilter_graph_segment_apply(seg, 0, &mut inputs, &mut outputs), "apply")?;
                if inputs.is_null() || outputs.is_null() || !(*inputs).next.is_null() || !(*outputs).next.is_null() {
                    anyhow::bail!("needs exactly one input and one output");
                }
                av::check(ff::avfilter_link(src, 0, (*inputs).filter_ctx, (*inputs).pad_idx as u32), "link input")?;
                av::check(ff::avfilter_link((*outputs).filter_ctx, (*outputs).pad_idx as u32, sink, 0), "link output")?;
                anyhow::Ok(())
            })();
            ff::avfilter_graph_segment_free(&mut seg);
            ff::avfilter_inout_free(&mut inputs);
            ff::avfilter_inout_free(&mut outputs);
            r.map_err(|e| anyhow::anyhow!("filter chain '{chain}': {e}"))?;
            av::check(ff::avfilter_graph_config(graph.0, ptr::null_mut()), "avfilter_graph_config")?;
            Ok(Graph { _graph: graph, src, sink })
        }
    }

    pub fn push(&mut self, frame: &mut Frame) -> anyhow::Result<()> {
        let r = unsafe { ff::av_buffersrc_add_frame_flags(self.src, frame.0, ff::AV_BUFFERSRC_FLAG_KEEP_REF as c_int) };
        av::check(r, "buffersrc")?;
        Ok(())
    }

    pub fn push_eof(&mut self) {
        unsafe {
            ff::av_buffersrc_add_frame_flags(self.src, ptr::null_mut(), 0);
        }
    }

    /// Pulls one filtered frame, if available.
    pub fn pull(&mut self, out: &mut Frame) -> bool {
        unsafe { ff::av_buffersink_get_frame(self.sink, out.0) >= 0 }
    }

    pub fn out_time_base(&self) -> ff::AVRational {
        unsafe { ff::av_buffersink_get_time_base(self.sink) }
    }
}
