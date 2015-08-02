// -------------------------------------------------------------------------------------------------
// Rick, a Rust intercal compiler.  Save your souls!
//
// Copyright (c) 2015 Georg Brandl
//
// This program is free software; you can redistribute it and/or modify it under the terms of the
// GNU General Public License as published by the Free Software Foundation; either version 2 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
// even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with this program;
// if not, write to the Free Software Foundation, Inc., 675 Mass Ave, Cambridge, MA 02139, USA.
// -------------------------------------------------------------------------------------------------

use std::fmt::{ Debug, Display };
use std::io::Write;
use std::u16;

use err::{ Res, IE123, IE129, IE275, IE633 };
use ast::{ self, Program, Stmt, StmtBody, Expr, Var, VType };
use stdops::{ Bind, Array, write_number, read_number, check_chance, check_ovf, pop_jumps,
              seed_chance, mingle, select, and_16, and_32, or_16, or_32, xor_16, xor_32 };


/// Type of an expression.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Val {
    I16(u16),
    I32(u32),
}

impl Val {
    /// Cast as a 16-bit value; returns an Error if 32-bit and too big.
    pub fn as_u16(&self) -> Res<u16> {
        match *self {
            Val::I16(v) => Ok(v),
            Val::I32(v) => {
                if v > (u16::MAX as u32) {
                    return IE275.err();
                }
                Ok(v as u16)
            }
        }
    }

    /// Cast as a 32-bit value; always succeeds.
    pub fn as_u32(&self) -> u32 {
        match *self {
            Val::I16(v) => v as u32,
            Val::I32(v) => v
        }
    }

    /// Cast as an usize value; always succeeds.
    pub fn as_usize(&self) -> usize {
        self.as_u32() as usize
    }

    /// Create from a 32-bit value; will select the smallest possible type.
    pub fn from_u32(v: u32) -> Val {
        if v & 0xFFFF == v {
            Val::I16(v as u16)
        } else {
            Val::I32(v)
        }
    }
}


pub struct Eval<'a> {
    program: &'a Program,
    stdout: &'a mut Write,
    debug: bool,
    spot: Vec<Bind<u16>>,
    twospot: Vec<Bind<u32>>,
    tail: Vec<Bind<Array<u16>>>,
    hybrid: Vec<Bind<Array<u32>>>,
    jumps: Vec<ast::LogLine>,
    abstain: Vec<u32>,
    last_in: u8,
    last_out: u8,
    stmt_ctr: usize,
}

enum StmtRes {
    Next,         // normal execution, next statement
    Jump(usize),  // DO ... NEXT
    Back(usize),  // RESUME
    FromTop,      // TRY AGAIN
    End,          // GIVE UP
}

impl<'a> Eval<'a> {
    pub fn new(program: &'a Program, stdout: &'a mut Write, debug: bool) -> Eval<'a> {
        let abs = program.stmts.iter().map(|stmt| stmt.props.disabled as u32).collect();
        let nvars = (program.var_info.0.len(),
                     program.var_info.1.len(),
                     program.var_info.2.len(),
                     program.var_info.3.len());
        Eval {
            program:  program,
            stdout:   stdout,
            debug:    debug,
            spot:     vec![Bind::new(0); nvars.0],
            twospot:  vec![Bind::new(0); nvars.1],
            tail:     vec![Bind::new(Array::empty()); nvars.2],
            hybrid:   vec![Bind::new(Array::empty()); nvars.3],
            jumps:    Vec::with_capacity(80),
            abstain:  abs,
            last_in:  0,
            last_out: 0,
            stmt_ctr: 0,
        }
    }

    pub fn eval(&mut self) -> Res<usize> {
        let mut pctr = 0;  // index of current statement
        let program = self.program.clone();
        let nstmts = program.stmts.len();
        seed_chance();
        loop {
            // check for falling off the end
            if pctr >= nstmts {
                // if the last statement was a TRY AGAIN, falling off the end is fine
                if let StmtBody::TryAgain = program.stmts[program.stmts.len() - 1].body {
                    break;
                }
                return IE633.err();
            }
            self.stmt_ctr += 1;
            // execute statement if not abstained
            if self.abstain[pctr] == 0 {
                let stmt = &program.stmts[pctr];
                // check execution chance
                if check_chance(stmt.props.chance) {
                    // try to eval this statement
                    let res = match self.eval_stmt(stmt) {
                        // on error, set the correct line number and bubble up
                        Err(mut err) => {
                            err.set_line(stmt.props.onthewayto);
                            // special treatment for Next
                            if let StmtBody::DoNext(n) = stmt.body {
                                if let Some(i) = program.labels.get(&n) {
                                    err.set_line(program.stmts[*i as usize].props.srcline);
                                }
                            }
                            return Err(err);
                        }
                        Ok(res)  => res
                    };
                    match res {
                        StmtRes::Next    => { }
                        StmtRes::Jump(n) => {
                            self.jumps.push(pctr as u16);  // push the line with the NEXT
                            pctr = n;
                            continue;  // do not increment or check for COME FROMs
                        }
                        StmtRes::Back(n) => {
                            pctr = n;  // will be incremented below after COME FROM check
                        }
                        StmtRes::FromTop => {
                            pctr = 0;  // start from the beginning, do not push any stack
                            continue;
                        }
                        StmtRes::End     => break,
                    }
                }
            }
            // check for COME FROMs from this line
            if let Some(next) = self.program.stmts[pctr].comefrom {
                // check for abstained COME FROM
                let next = next as usize;
                if self.abstain[next] == 0 {
                    if check_chance(program.stmts[next].props.chance) {
                        pctr = next;
                        continue;
                    }
                }
            }
            // no COME FROM, normal execution
            pctr += 1;
        }
        Ok(self.stmt_ctr)
    }

    /// Process a single statement.
    fn eval_stmt(&mut self, stmt: &Stmt) -> Res<StmtRes> {
        if self.debug {
            println!("\nExecuting Stmt #{} (state before following)", self.stmt_ctr);
            self.dump_state();
            println!("{}", stmt);
        }
        match stmt.body {
            StmtBody::Calc(ref var, ref expr) => {
                let val = try!(self.eval_expr(expr));
                try!(self.assign(var, val));
                Ok(StmtRes::Next)
            }
            StmtBody::Dim(ref var, ref exprs) => {
                try!(self.array_dim(var, exprs));
                Ok(StmtRes::Next)
            }
            StmtBody::DoNext(n) => {
                let j = self.jumps.len();
                match self.program.labels.get(&n) {
                    // too many jumps on stack already?
                    Some(_) if j >= 80 => IE123.err(),
                    Some(i)            => Ok(StmtRes::Jump(*i as usize)),
                    None               => IE129.err(),
                }
            }
            StmtBody::ComeFrom(_) => {
                // nothing to do here at runtime
                Ok(StmtRes::Next)
            }
            StmtBody::Resume(ref expr) => {
                let n = try!(self.eval_expr(expr)).as_u32();
                let next = try!(pop_jumps(&mut self.jumps, n, true)).unwrap();
                Ok(StmtRes::Back(next as usize))
            }
            StmtBody::Forget(ref expr) => {
                let n = try!(self.eval_expr(expr)).as_u32();
                try!(pop_jumps(&mut self.jumps, n, false));
                Ok(StmtRes::Next)
            }
            StmtBody::Ignore(ref vars) => {
                for var in vars {
                    self.set_rw(var, false);
                }
                Ok(StmtRes::Next)
            }
            StmtBody::Remember(ref vars) => {
                for var in vars {
                    self.set_rw(var, true);
                }
                Ok(StmtRes::Next)
            }
            StmtBody::Stash(ref vars) => {
                for var in vars {
                    self.stash(var);
                }
                Ok(StmtRes::Next)
            }
            StmtBody::Retrieve(ref vars) => {
                for var in vars {
                    try!(self.retrieve(var));
                }
                Ok(StmtRes::Next)
            }
            StmtBody::Abstain(ref expr, ref whats) => {
                let f: Box<Fn(u32) -> u32> = if let Some(ref e) = *expr {
                    let n = try!(self.eval_expr(e)).as_u32();
                    box move |v: u32| v.saturating_add(n)
                } else {
                    box |_| 1
                };
                for what in whats {
                    self.abstain(what, &*f);
                }
                Ok(StmtRes::Next)
            }
            StmtBody::Reinstate(ref whats) => {
                for what in whats {
                    self.abstain(what, &|v: u32| v.saturating_sub(1));
                }
                Ok(StmtRes::Next)
            }
            StmtBody::ReadOut(ref vars) => {
                for var in vars {
                    match *var {
                        Expr::Var(ref var) if var.is_dim() => {
                            try!(self.array_readout(var));
                        }
                        Expr::Var(ref var) => {
                            let varval = try!(self.lookup(var));
                            write_number(self.stdout, varval.as_u32());
                        }
                        Expr::Num(_, v) => write_number(self.stdout, v),
                        _ => unreachable!(),
                    };
                }
                Ok(StmtRes::Next)
            }
            StmtBody::WriteIn(ref vars) => {
                for var in vars {
                    if var.is_dim() {
                        try!(self.array_writein(var));
                    } else {
                        let n = try!(read_number(0));
                        try!(self.assign(var, Val::from_u32(n)));
                    }
                }
                Ok(StmtRes::Next)
            }
            StmtBody::Print(ref s) => {
                write!(self.stdout, "{}", s).unwrap();
                Ok(StmtRes::Next)
            }
            StmtBody::TryAgain => Ok(StmtRes::FromTop),
            StmtBody::GiveUp => Ok(StmtRes::End),
            StmtBody::Error(ref e) => Err((*e).clone()),
        }
    }

    /// Evaluate an expression to a value.
    fn eval_expr(&self, expr: &Expr) -> Res<Val> {
        match *expr {
            Expr::Num(vtype, v) => match vtype {
                VType::I16 => Ok(Val::I16(v as u16)),
                VType::I32 => Ok(Val::I32(v)),
            },
            Expr::Var(ref var) => self.lookup(var),
            Expr::Mingle(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx)).as_u32();
                let w = try!(self.eval_expr(wx)).as_u32();
                let v = try!(check_ovf(v, 0));
                let w = try!(check_ovf(w, 0));
                Ok(Val::I32(mingle(v, w)))
            }
            Expr::Select(vtype, ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                if vtype == VType::I16 {
                    Ok(Val::I16(select(v.as_u32(), try!(w.as_u16()) as u32) as u16))
                } else {
                    Ok(Val::I32(select(v.as_u32(), w.as_u32())))
                }
            }
            Expr::And(vtype, ref vx) => {
                let v = try!(self.eval_expr(vx));
                match vtype {
                    VType::I16 => Ok(Val::I16(and_16(try!(v.as_u16()) as u32) as u16)),
                    VType::I32 => Ok(Val::I32(and_32(v.as_u32()))),
                }
            }
            Expr::Or(vtype, ref vx) => {
                let v = try!(self.eval_expr(vx));
                match vtype {
                    VType::I16 => Ok(Val::I16(or_16(try!(v.as_u16()) as u32) as u16)),
                    VType::I32 => Ok(Val::I32(or_32(v.as_u32()))),
                }
            }
            Expr::Xor(vtype, ref vx) => {
                let v = try!(self.eval_expr(vx));
                match vtype {
                    VType::I16 => Ok(Val::I16(xor_16(try!(v.as_u16()) as u32) as u16)),
                    VType::I32 => Ok(Val::I32(xor_32(v.as_u32()))),
                }
            }
            Expr::RsNot(ref vx) => {
                let v = try!(self.eval_expr(vx));
                Ok(Val::I32(!v.as_u32()))
            }
            Expr::RsAnd(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32(v.as_u32() & w.as_u32()))
            }
            Expr::RsOr(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32(v.as_u32() | w.as_u32()))
            }
            Expr::RsXor(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32(v.as_u32() ^ w.as_u32()))
            }
            Expr::RsRshift(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32(v.as_u32() >> w.as_u32()))
            }
            Expr::RsLshift(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32(v.as_u32() << w.as_u32()))
            }
            // Expr::RsEqual(ref vx, ref wx) => {
            //     let v = try!(self.eval_expr(vx));
            //     let w = try!(self.eval_expr(wx));
            //     Ok(Val::I32((v.as_u32() == w.as_u32()) as u32))
            // }
            Expr::RsNotEqual(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32((v.as_u32() != w.as_u32()) as u32))
            }
            Expr::RsPlus(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32(v.as_u32() + w.as_u32()))
            }
            Expr::RsMinus(ref vx, ref wx) => {
                let v = try!(self.eval_expr(vx));
                let w = try!(self.eval_expr(wx));
                Ok(Val::I32(v.as_u32() - w.as_u32()))
            }
        }
    }

    #[inline]
    fn eval_subs(&self, subs: &Vec<Expr>) -> Res<Vec<usize>> {
        subs.iter().map(|v| self.eval_expr(v).map(|w| w.as_usize())).collect()
    }

    /// Dimension an array.
    fn array_dim(&mut self, var: &Var, dims: &Vec<Expr>) -> Res<()> {
        let dims = try!(self.eval_subs(dims));
        match *var {
            Var::A16(n, _) => self.tail[n].dimension(dims),
            Var::A32(n, _) => self.hybrid[n].dimension(dims),
            _ => unimplemented!()
        }
    }

    /// Assign to a variable.
    fn assign(&mut self, var: &Var, val: Val) -> Res<()> {
        //println!("assign: {:?} = {}", var, val.as_u32());
        match *var {
            Var::I16(n) => Ok(self.spot[n].assign(try!(val.as_u16()))),
            Var::I32(n) => Ok(self.twospot[n].assign(val.as_u32())),
            Var::A16(n, ref subs) => {
                let subs = try!(self.eval_subs(subs));
                self.tail[n].set_md(subs, try!(val.as_u16()))
            }
            Var::A32(n, ref subs) => {
                let subs = try!(self.eval_subs(subs));
                self.hybrid[n].set_md(subs, val.as_u32())
            }
        }
    }

    /// Look up the value of a variable.
    fn lookup(&self, var: &Var) -> Res<Val> {
        match *var {
            Var::I16(n) => Ok(Val::I16(self.spot[n].val)),
            Var::I32(n) => Ok(Val::I32(self.twospot[n].val)),
            Var::A16(n, ref subs) => {
                let subs = try!(self.eval_subs(subs));
                self.tail[n].get_md(subs).map(Val::I16)
            }
            Var::A32(n, ref subs) => {
                let subs = try!(self.eval_subs(subs));
                self.hybrid[n].get_md(subs).map(Val::I32)
            }
        }
    }

    /// Process a STASH statement.
    fn stash(&mut self, var: &Var) {
        match *var {
            Var::I16(n) => self.spot[n].stash(),
            Var::I32(n) => self.twospot[n].stash(),
            Var::A16(n, _) => self.tail[n].stash(),
            Var::A32(n, _) => self.hybrid[n].stash(),
        }
    }

    /// Process a RETRIEVE statement.
    fn retrieve(&mut self, var: &Var) -> Res<()> {
        match *var {
            Var::I16(n) => self.spot[n].retrieve(),
            Var::I32(n) => self.twospot[n].retrieve(),
            Var::A16(n, _) => self.tail[n].retrieve(),
            Var::A32(n, _) => self.hybrid[n].retrieve(),
        }
    }

    /// Process an IGNORE or REMEMBER statement.  Cannot fail.
    fn set_rw(&mut self, var: &Var, rw: bool) {
        match *var {
            Var::I16(n) => self.spot[n].rw = rw,
            Var::I32(n) => self.twospot[n].rw = rw,
            Var::A16(n, _) => self.tail[n].rw = rw,
            Var::A32(n, _) => self.hybrid[n].rw = rw,
        }
    }

    /// P()rocess an ABSTAIN or REINSTATE statement.  Cannot fail.
    fn abstain(&mut self, what: &ast::Abstain, f: &Fn(u32) -> u32) {
        if let &ast::Abstain::Label(lbl) = what {
            let idx = self.program.labels[&lbl] as usize;
            if self.program.stmts[idx].body != StmtBody::GiveUp {
                self.abstain[idx] = f(self.abstain[idx]);
            }
        } else {
            for (i, stype) in self.program.stmt_types.iter().enumerate() {
                if stype == what {
                    self.abstain[i] = f(self.abstain[i]);
                }
            }
        }
    }

    /// Array readout helper.
    fn array_readout(&mut self, var: &Var) -> Res<()> {
        let state = &mut self.last_out;
        match *var {
            Var::A16(n, _) => self.tail[n].readout(self.stdout, state),
            Var::A32(n, _) => self.hybrid[n].readout(self.stdout, state),
            _ => unimplemented!()
        }
    }

    /// Array writein helper.
    fn array_writein(&mut self, var: &Var) -> Res<()> {
        let state = &mut self.last_in;
        match *var {
            Var::A16(n, _) => self.tail[n].writein(state),
            Var::A32(n, _) => self.hybrid[n].writein(state),
            _ => unimplemented!()
        }
    }

    /// Debug helper.
    fn dump_state(&self) {
        self.dump_state_one(&self.spot, ".");
        self.dump_state_one(&self.twospot, ":");
        self.dump_state_one(&self.tail, ",");
        self.dump_state_one(&self.hybrid, ";");
        if self.jumps.len() > 0 {
            println!("Next stack: {:?}", self.jumps);
        }
        //println!("Abstained: {:?}", self.abstain);
    }

    fn dump_state_one<T: Debug + Display>(&self, vec: &Vec<Bind<T>>, sigil: &str) {
        if vec.len() > 0 {
            for (i, v) in vec.iter().enumerate() {
                print!("{}{} = {}, ", sigil, i, v);
            }
            println!("");
        }
    }
}
