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
            let mut outputs = ff::avfilter_inout_alloc();
            let mut inputs = ff::avfilter_inout_alloc();
            (*outputs).name = ff::av_strdup(in_name.as_ptr());
            (*outputs).filter_ctx = src;
            (*outputs).pad_idx = 0;
            (*outputs).next = ptr::null_mut();
            (*inputs).name = ff::av_strdup(out_name.as_ptr());
            (*inputs).filter_ctx = sink;
            (*inputs).pad_idx = 0;
            (*inputs).next = ptr::null_mut();
            let desc = av::cstr(chain);
            let r = ff::avfilter_graph_parse_ptr(graph.0, desc.as_ptr(), &mut inputs, &mut outputs, ptr::null_mut());
            ff::avfilter_inout_free(&mut inputs);
            ff::avfilter_inout_free(&mut outputs);
            av::check(r, &format!("filter chain '{chain}'"))?;
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
