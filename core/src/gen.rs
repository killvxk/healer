use std::collections::HashMap;
use std::path::PathBuf;

use bitset_fixed::BitSet;
use ndarray::Axis;
use rand::distributions::{Alphanumeric, Standard};
use rand::prelude::*;
use rand::{random, thread_rng, Rng};

use fots::types::{
    Field, Flag, FnInfo, GroupId, NumInfo, NumLimit, PtrDir, StrType, TypeId, TypeInfo,
};

use crate::analyze::{RTable, Relation};
use crate::prog::{Arg, ArgIndex, ArgPos, Call, Prog};
use crate::target::Target;
use crate::value::{NumValue, Value};

pub struct Config {
    pub prog_max_len: usize,
    pub prog_min_len: usize,
    pub str_min_len: usize,
    pub str_max_len: usize,
    pub path_max_depth: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prog_max_len: 15,
            prog_min_len: 3,
            str_min_len: 4,
            str_max_len: 128,
            path_max_depth: 4,
        }
    }
}

pub fn gen<S: std::hash::BuildHasher>(
    t: &Target,
    rs: &HashMap<GroupId, RTable, S>,
    conf: &Config,
) -> Prog {
    assert!(!rs.is_empty());
    assert_eq!(t.groups.len(), rs.len());
    let mut rng = thread_rng();

    // choose group
    let gid = rs.keys().choose(&mut rng).unwrap();
    let g = &t.groups[gid];

    // choose sequence
    let r = &rs[gid];
    let seq = choose_seq(r, conf);

    // gen value
    let mut s = State::new(Prog::new(*gid), conf);
    for &i in seq.iter() {
        gen_call(t, &g.fns[i], &mut s);
    }
    s.prog
}

struct State<'a> {
    res: HashMap<TypeId, Vec<ArgIndex>>,
    strs: HashMap<StrType, Vec<String>>,
    prog: Prog,
    conf: &'a Config,
}

impl<'a> State<'a> {
    pub fn new(prog: Prog, conf: &'a Config) -> Self {
        Self {
            res: HashMap::new(),
            strs: hashmap! {StrType::FileName => Vec::new()},
            prog,
            conf,
        }
    }

    pub fn record_res(&mut self, tid: TypeId, is_ret: bool) {
        let cid = self.prog.len() - 1;
        let arg_pos = self.prog.calls[cid].args.len() - 1;

        let idx = self.res.entry(tid).or_insert_with(Default::default);
        if is_ret {
            idx.push((cid, ArgPos::Ret))
        } else {
            idx.push((cid, ArgPos::Arg(arg_pos)))
        }
    }

    pub fn record_str(&mut self, t: StrType, val: &str) {
        let vals = self.strs.entry(t).or_insert_with(Default::default);
        vals.push(val.into())
    }

    pub fn try_reuse_res(&self, tid: TypeId) -> Option<Value> {
        let mut rng = thread_rng();
        if let Some(res) = self.res.get(&tid) {
            if !res.is_empty() {
                let r = res.choose(&mut rng).unwrap();
                return Some(Value::Ref(r.clone()));
            }
        }
        None
    }

    pub fn try_reuse_str(&self, str_type: StrType) -> Option<Value> {
        let mut rng = thread_rng();
        if let Some(strs) = self.strs.get(&str_type) {
            if !strs.is_empty() && rng.gen() {
                let s = strs.choose(&mut rng).unwrap();
                return Some(Value::Str(s.clone()));
            }
        }
        None
    }

    // add call
    #[inline]
    pub fn add_call(&mut self, call: Call) -> &mut Call {
        self.prog.add_call(call)
    }

    // Add arg for last call
    #[inline]
    pub fn add_arg(&mut self, arg: Arg) -> &mut Arg {
        let i = self.prog.len() - 1;
        self.prog.calls[i].add_arg(arg)
    }

    #[inline]
    pub fn add_ret(&mut self, arg: Arg) -> &mut Arg {
        let i = self.prog.len() - 1;
        self.prog.calls[i].ret = Some(arg);
        self.prog.calls[i].ret.as_mut().unwrap()
    }

    #[inline]
    pub fn update_val(&mut self, val: Value) {
        let c = self.prog.calls.last_mut().unwrap();
        let arg_index = c.args.len() - 1;
        c.args[arg_index].val = val;
    }
}

fn gen_call(t: &Target, f: &FnInfo, s: &mut State) {
    s.add_call(Call::new(f.id));

    if f.has_params() {
        for p in f.iter_param() {
            s.add_arg(Arg::new(p.tid));
            let val = gen_value(p.tid, t, s);
            s.update_val(val);
        }
    }

    if let Some(tid) = f.r_tid {
        if t.is_res(tid) {
            s.add_ret(Arg::new(tid));
            s.record_res(tid, true);
        }
    }
}

fn gen_value(tid: TypeId, t: &Target, s: &mut State) -> Value {
    match t.type_of(tid) {
        TypeInfo::Num(num_info) => gen_num(num_info),
        TypeInfo::Ptr { dir, tid, depth } => {
            assert!(*depth == 1, "Multi-level pointer not supported");
            gen_ptr(*dir, *tid, t, s)
        }
        TypeInfo::Slice { tid, l, h } => gen_slice(*tid, *l, *h, t, s),
        TypeInfo::Str { str_type, vals } => gen_str(str_type, vals, s),
        TypeInfo::Struct { fields, .. } => gen_struct(&fields[..], t, s),
        TypeInfo::Union { fields, .. } => gen_union(&fields[..], t, s),
        TypeInfo::Flag { flags, .. } => gen_flag(&flags[..]),
        TypeInfo::Alias { tid: under_id, .. } => gen_alias(tid, *under_id, t, s),
        TypeInfo::Res { tid: under_tid } => gen_res(tid, *under_tid, t, s),
        TypeInfo::Len {
            tid: _tid,
            path: _p,
            is_param: _is_param,
        } => Value::Num(NumValue::Unsigned(0)),
    }
}

fn gen_alias(tid: TypeId, under_id: TypeId, t: &Target, s: &mut State) -> Value {
    if t.is_res(tid) {
        gen_res(tid, under_id, t, s)
    } else {
        gen_value(under_id, t, s)
    }
}

fn gen_res(res_tid: TypeId, tid: TypeId, t: &Target, s: &mut State) -> Value {
    if let Some(res) = s.try_reuse_res(res_tid) {
        res
    } else {
        gen_value(tid, t, s)
    }
}

fn gen_ptr(dir: PtrDir, tid: TypeId, t: &Target, s: &mut State) -> Value {
    if dir != PtrDir::In {
        if t.is_res(tid) {
            s.record_res(tid, false);
        }
        return Value::default_val(tid, t);
    }

    if thread_rng().gen::<f64>() >= 0.1 {
        gen_value(tid, t, s)
    } else {
        Value::None
    }
}

fn gen_flag(flags: &[Flag]) -> Value {
    assert!(!flags.is_empty());

    let mut rng = thread_rng();

    if rng.gen::<f64>() >= 0.8 {
        Value::Num(NumValue::Signed(rng.gen::<i32>() as i64))
    } else {
        let flag = flags.iter().choose(&mut rng).unwrap();
        let mut val = flag.val;

        loop {
            if rng.gen() {
                let flag = flags.iter().choose(&mut rng).unwrap();
                val &= flag.val;
            } else {
                break;
            }
        }
        Value::Num(NumValue::Signed(val))
    }
}

fn gen_union(fields: &[Field], t: &Target, s: &mut State) -> Value {
    assert!(!fields.is_empty());

    let i = thread_rng().gen_range(0, fields.len());
    let field = &fields[i];

    Value::Opt {
        choice: i,
        val: Box::new(gen_value(field.tid, t, s)),
    }
}

fn gen_struct(fields: &[Field], t: &Target, s: &mut State) -> Value {
    let mut vals = Vec::new();
    for field in fields.iter() {
        vals.push(gen_value(field.tid, t, s));
    }
    Value::Group(vals)
}

fn gen_str(str_type: &StrType, vals: &Option<Vec<String>>, s: &mut State) -> Value {
    let mut rng = thread_rng();
    if let Some(vals) = vals {
        if !vals.is_empty() {
            return Value::Str(vals.choose(&mut rng).unwrap().clone());
        }
    }

    let len = rng.gen_range(s.conf.str_min_len, s.conf.str_max_len);
    match str_type {
        StrType::Str => {
            if let Some(s) = s.try_reuse_str(StrType::Str) {
                return s;
            }
            let val = rng
                .sample_iter::<char, Standard>(Standard)
                .take(len)
                .collect::<String>();
            s.record_str(StrType::Str, &val);
            Value::Str(val)
        }
        StrType::CStr => {
            if let Some(s) = s.try_reuse_str(StrType::Str) {
                return s;
            }
            let val = rng.sample_iter(Alphanumeric).take(len).collect::<String>();
            s.record_str(StrType::CStr, &val);
            Value::Str(val)
        }
        StrType::FileName => {
            if let Some(v) = s.try_reuse_str(StrType::FileName) {
                return v;
            }
            let mut path = PathBuf::from(".");
            let mut depth = 0;
            loop {
                let sub_path = rng.sample_iter(Alphanumeric).take(len).collect::<String>();
                path.push(sub_path);
                depth += 1;
                if depth < s.conf.path_max_depth && rng.gen::<f64>() > 0.4 {
                    continue;
                } else if let Ok(p) = path.into_os_string().into_string() {
                    s.record_str(StrType::FileName, &p);
                    return Value::Str(p);
                } else {
                    path = PathBuf::from(".");
                    depth = 0;
                }
            }
        }
    }
}

fn gen_slice(tid: TypeId, l: isize, h: isize, t: &Target, s: &mut State) -> Value {
    let len: usize = gen_slice_len(l, h);
    let mut vals = Vec::new();

    for _ in 0..len {
        vals.push(gen_value(tid, t, s));
    }
    Value::Group(vals)
}

pub(crate) fn gen_slice_len(l: isize, h: isize) -> usize {
    match (l, h) {
        (-1, -1) => thread_rng().gen_range(0, 8),
        (l, -1) => thread_rng().gen_range(0, l as usize),
        (l, h) => thread_rng().gen_range(l as usize, h as usize),
    }
}

fn gen_num(type_info: &NumInfo) -> Value {
    let mut rng = thread_rng();

    match type_info {
        NumInfo::I8(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Signed(*vals.choose(&mut rng).unwrap() as i64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Signed(rng.gen_range(r.start, r.end) as i64))
            }
            NumLimit::None => Value::Num(NumValue::Signed(rng.gen::<i8>() as i64)),
        },
        NumInfo::I16(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Signed(*vals.choose(&mut rng).unwrap() as i64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Signed(rng.gen_range(r.start, r.end) as i64))
            }
            NumLimit::None => Value::Num(NumValue::Signed(rng.gen::<i16>() as i64)),
        },
        NumInfo::I32(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Signed(*vals.choose(&mut rng).unwrap() as i64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Signed(rng.gen_range(r.start, r.end) as i64))
            }
            NumLimit::None => Value::Num(NumValue::Signed(rng.gen::<i32>() as i64)),
        },
        NumInfo::I64(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Signed(*vals.choose(&mut rng).unwrap() as i64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Signed(rng.gen_range(r.start, r.end) as i64))
            }
            NumLimit::None => Value::Num(NumValue::Signed(rng.gen::<i64>() as i64)),
        },
        NumInfo::U8(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Unsigned(*vals.choose(&mut rng).unwrap() as u64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Unsigned(rng.gen_range(r.start, r.end) as u64))
            }
            NumLimit::None => Value::Num(NumValue::Unsigned(rng.gen::<u8>() as u64)),
        },
        NumInfo::U16(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Unsigned(*vals.choose(&mut rng).unwrap() as u64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Unsigned(rng.gen_range(r.start, r.end) as u64))
            }
            NumLimit::None => Value::Num(NumValue::Unsigned(rng.gen::<u16>() as u64)),
        },
        NumInfo::U32(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Unsigned(*vals.choose(&mut rng).unwrap() as u64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Unsigned(rng.gen_range(r.start, r.end) as u64))
            }
            NumLimit::None => Value::Num(NumValue::Unsigned(rng.gen::<u32>() as u64)),
        },
        NumInfo::U64(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Unsigned(*vals.choose(&mut rng).unwrap() as u64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Unsigned(rng.gen_range(r.start, r.end) as u64))
            }
            NumLimit::None => Value::Num(NumValue::Unsigned(rng.gen::<u64>() as u64)),
        },
        NumInfo::Usize(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Unsigned(*vals.choose(&mut rng).unwrap() as u64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Unsigned(rng.gen_range(r.start, r.end) as u64))
            }
            NumLimit::None => Value::Num(NumValue::Unsigned(rng.gen::<usize>() as u64)),
        },
        NumInfo::Isize(l) => match l {
            NumLimit::Vals(vals) => {
                Value::Num(NumValue::Signed(*vals.choose(&mut rng).unwrap() as i64))
            }
            NumLimit::Range(r) => {
                Value::Num(NumValue::Signed(rng.gen_range(r.start, r.end) as i64))
            }
            NumLimit::None => Value::Num(NumValue::Signed(rng.gen::<isize>() as i64)),
        },
    }
}

fn choose_seq(rs: &RTable, conf: &Config) -> Vec<usize> {
    assert!(!rs.is_empty());

    let mut rng = thread_rng();
    let mut set = BitSet::new(rs.len());
    let mut seq = Vec::new();

    loop {
        let index = rng.gen_range(0, rs.len());
        set.set(index, true);
        seq.push(index);
        let i = seq.len() - 1;
        push_deps(rs, &mut set, &mut seq, i, conf);

        if seq.len() <= conf.prog_max_len && rng.gen() {
            continue;
        } else {
            break;
        }
    }
    seq
}

fn push_deps(rs: &RTable, set: &mut BitSet, seq: &mut Vec<usize>, i: usize, conf: &Config) {
    if i >= seq.len() || seq.len() >= conf.prog_max_len {
        return;
    }
    let index = seq[i];
    let mut deps = Vec::new();
    for (j, r) in rs.index_axis(Axis(0), index).iter().enumerate() {
        if r.eq(&Relation::Some) {
            if !set[j] && random::<f64>() > 0.25 {
                deps.push(j);
                set.set(j, true);
            } else if set[j] && random::<f64>() > 0.75 {
                deps.push(j);
            }
        }

        if r.eq(&Relation::Unknown) {
            if !set[j] && random() {
                deps.push(j);
                set.set(j, true);
            } else if set[j] && random::<f64>() > 0.875 {
                deps.push(j);
            }
        }
    }
    seq.extend(deps);
    push_deps(rs, set, seq, i + 1, conf);
}