// Copyright 2023 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use byteorder::BigEndian;
use byteorder::WriteBytesExt;

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::VecDeque;

use crate::constants::*;
use crate::jsonpath::ArrayIndex;
use crate::jsonpath::BinaryOperator;
use crate::jsonpath::Expr;
use crate::jsonpath::FilterFunc;
use crate::jsonpath::Index;
use crate::jsonpath::JsonPath;
use crate::jsonpath::Path;
use crate::jsonpath::PathValue;
use crate::number::Number;
use crate::Error;
use crate::OwnedJsonb;
use crate::RawJsonb;

use nom::{
    bytes::complete::take, combinator::map, multi::count, number::complete::be_u32, IResult,
};

/// The position of jsonb value.
#[derive(Clone, Debug)]
enum Position {
    /// The offset and length of jsonb container value.
    Container((usize, usize)),
    /// The type, offset and length of jsonb scalar value.
    Scalar((u32, usize, usize)),
}

#[derive(Debug)]
enum ExprValue<'a> {
    Values(Vec<PathValue<'a>>),
    Value(Box<PathValue<'a>>),
}

/// Mode determines the different forms of the return value.
#[derive(Clone, PartialEq, Debug)]
pub enum Mode {
    /// Only return the first jsonb value.
    First,
    /// Return all values as a jsonb Array.
    Array,
    /// Return each jsonb value separately.
    All,
    /// If there are multiple values, return a jsonb Array,
    /// if there is only one value, return the jsonb value directly.
    Mixed,
}

pub struct Selector<'a> {
    json_path: &'a JsonPath<'a>,
    mode: Mode,
}

impl<'a> Selector<'a> {
    pub fn new(json_path: &'a JsonPath<'a>, mode: Mode) -> Self {
        Self { json_path, mode }
    }

    pub fn select(&'a self, root: RawJsonb) -> Result<Vec<OwnedJsonb>, Error> {
        let mut poses = self.find_positions(root, None, &self.json_path.paths)?;

        if self.json_path.is_predicate() {
            let owned_jsonbs = Self::build_predicate_result(&mut poses)?;
            return Ok(owned_jsonbs);
        }

        let owned_jsonbs = match self.mode {
            Mode::All => Self::build_values(root, &mut poses)?,
            Mode::First => {
                poses.truncate(1);
                Self::build_values(root, &mut poses)?
            }
            Mode::Array => Self::build_scalar_array(root, &mut poses)?,
            Mode::Mixed => {
                if poses.len() > 1 {
                    Self::build_scalar_array(root, &mut poses)?
                } else {
                    Self::build_values(root, &mut poses)?
                }
            }
        };
        Ok(owned_jsonbs)
    }

    pub fn exists(&'a self, root: RawJsonb) -> Result<bool, Error> {
        if self.json_path.is_predicate() {
            return Ok(true);
        }
        let poses = self.find_positions(root, None, &self.json_path.paths)?;
        Ok(!poses.is_empty())
    }

    pub fn predicate_match(&'a self, root: RawJsonb) -> Result<bool, Error> {
        if !self.json_path.is_predicate() {
            return Err(Error::InvalidJsonPathPredicate);
        }
        let poses = self.find_positions(root, None, &self.json_path.paths)?;
        Ok(!poses.is_empty())
    }

    fn find_positions(
        &'a self,
        root: RawJsonb,
        current: Option<&Position>,
        paths: &[Path<'a>],
    ) -> Result<VecDeque<Position>, Error> {
        let mut poses = VecDeque::new();

        let start_pos = if let Some(Path::Current) = paths.first() {
            current.expect("missing current position").clone()
        } else {
            Position::Container((0, root.len()))
        };
        poses.push_back(start_pos);

        for path in paths.iter() {
            match path {
                &Path::Root | &Path::Current => {
                    continue;
                }
                Path::FilterExpr(expr) | Path::Predicate(expr) => {
                    let len = poses.len();
                    for _ in 0..len {
                        let pos = poses.pop_front().unwrap();
                        let res = self.filter_expr(root, &pos, expr)?;
                        if res {
                            poses.push_back(pos);
                        }
                    }
                }
                _ => {
                    let len = poses.len();
                    for _ in 0..len {
                        let pos = poses.pop_front().unwrap();
                        match pos {
                            Position::Container((offset, length)) => {
                                self.select_path(root, offset, length, path, &mut poses)?;
                            }
                            Position::Scalar(_) => {
                                // In lax mode, bracket wildcard allow Scalar value.
                                if path == &Path::BracketWildcard {
                                    poses.push_back(pos);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(poses)
    }

    fn select_path(
        &'a self,
        root: RawJsonb,
        offset: usize,
        length: usize,
        path: &Path<'a>,
        poses: &mut VecDeque<Position>,
    ) -> Result<(), Error> {
        match path {
            Path::DotWildcard => {
                self.select_object_values(root, offset, poses)?;
            }
            Path::BracketWildcard => {
                self.select_array_values(root, offset, length, poses)?;
            }
            Path::ColonField(name) | Path::DotField(name) | Path::ObjectField(name) => {
                self.select_by_name(root, offset, name, poses)?;
            }
            Path::ArrayIndices(indices) => {
                self.select_by_indices(root, offset, indices, poses)?;
            }
            _ => unreachable!(),
        }
        Ok(())
    }

    // select all values in an Object.
    fn select_object_values(
        &'a self,
        root: RawJsonb,
        root_offset: usize,
        poses: &mut VecDeque<Position>,
    ) -> Result<(), Error> {
        let (rest, (ty, length)) = decode_header(&root.data[root_offset..])?;
        if ty != OBJECT_CONTAINER_TAG || length == 0 {
            return Ok(());
        }
        let (rest, key_jentries) = decode_jentries(rest, length)?;
        let (_, val_jentries) = decode_jentries(rest, length)?;
        let mut offset = root_offset + 4 + length * 8;
        for (_, length) in key_jentries.iter() {
            offset += length;
        }
        for (jty, jlength) in val_jentries.iter() {
            let pos = if *jty == CONTAINER_TAG {
                Position::Container((offset, *jlength))
            } else {
                Position::Scalar((*jty, offset, *jlength))
            };
            poses.push_back(pos);
            offset += jlength;
        }
        Ok(())
    }

    // select all values in an Array.
    fn select_array_values(
        &'a self,
        root: RawJsonb,
        root_offset: usize,
        root_length: usize,
        poses: &mut VecDeque<Position>,
    ) -> Result<(), Error> {
        let (rest, (ty, length)) = decode_header(&root.data[root_offset..])?;
        if ty != ARRAY_CONTAINER_TAG {
            // In lax mode, bracket wildcard allow Scalar value.
            poses.push_back(Position::Container((root_offset, root_length)));
            return Ok(());
        }
        let (_, val_jentries) = decode_jentries(rest, length)?;
        let mut offset = root_offset + 4 + length * 4;
        for (jty, jlength) in val_jentries.iter() {
            let pos = if *jty == CONTAINER_TAG {
                Position::Container((offset, *jlength))
            } else {
                Position::Scalar((*jty, offset, *jlength))
            };
            poses.push_back(pos);
            offset += jlength;
        }
        Ok(())
    }

    // select value in an Object by key name.
    fn select_by_name(
        &'a self,
        root: RawJsonb,
        root_offset: usize,
        name: &str,
        poses: &mut VecDeque<Position>,
    ) -> Result<(), Error> {
        let (rest, (ty, length)) = decode_header(&root.data[root_offset..])?;
        if ty != OBJECT_CONTAINER_TAG || length == 0 {
            return Ok(());
        }
        let (rest, key_jentries) = decode_jentries(rest, length)?;
        let (_, val_jentries) = decode_jentries(rest, length)?;
        let mut idx = 0;
        let mut offset = root_offset + 4 + length * 8;
        let mut found = false;
        for (i, (_, jlength)) in key_jentries.iter().enumerate() {
            if name.len() != *jlength || found {
                offset += jlength;
                continue;
            }
            let (_, key) = decode_string(&root.data[offset..], *jlength)?;
            if name == unsafe { std::str::from_utf8_unchecked(key) } {
                found = true;
                idx = i;
            }
            offset += jlength;
        }
        if !found {
            return Ok(());
        }
        for (i, (jty, jlength)) in val_jentries.iter().enumerate() {
            if i != idx {
                offset += jlength;
                continue;
            }
            let pos = if *jty == CONTAINER_TAG {
                Position::Container((offset, *jlength))
            } else {
                Position::Scalar((*jty, offset, *jlength))
            };
            poses.push_back(pos);
            break;
        }
        Ok(())
    }

    // select values in an Array by indices.
    fn select_by_indices(
        &'a self,
        root: RawJsonb,
        root_offset: usize,
        indices: &Vec<ArrayIndex>,
        poses: &mut VecDeque<Position>,
    ) -> Result<(), Error> {
        let (rest, (ty, length)) = decode_header(&root.data[root_offset..])?;
        if ty != ARRAY_CONTAINER_TAG || length == 0 {
            return Ok(());
        }
        let mut val_indices = Vec::new();
        for index in indices {
            match index {
                ArrayIndex::Index(idx) => {
                    if let Some(idx) = Self::convert_index(idx, length as i32) {
                        val_indices.push(idx);
                    }
                }
                ArrayIndex::Slice((start, end)) => {
                    if let Some(mut idxes) = Self::convert_slice(start, end, length as i32) {
                        val_indices.append(&mut idxes);
                    }
                }
            }
        }
        if val_indices.is_empty() {
            return Ok(());
        }
        let (_, jentries) = decode_jentries(rest, length)?;
        let mut offset = root_offset + 4 + length * 4;
        let mut offsets = Vec::with_capacity(jentries.len());
        for (_, jlength) in jentries.iter() {
            offsets.push(offset);
            offset += jlength;
        }
        for i in val_indices {
            let offset = offsets[i];
            let (jty, jlength) = jentries[i];
            let pos = if jty == CONTAINER_TAG {
                Position::Container((offset, jlength))
            } else {
                Position::Scalar((jty, offset, jlength))
            };
            poses.push_back(pos);
        }
        Ok(())
    }

    fn build_predicate_result(poses: &mut VecDeque<Position>) -> Result<Vec<OwnedJsonb>, Error> {
        let jentry = match poses.pop_front() {
            Some(_) => TRUE_TAG,
            None => FALSE_TAG,
        };
        let mut data = Vec::with_capacity(8);
        data.write_u32::<BigEndian>(SCALAR_CONTAINER_TAG)?;
        data.write_u32::<BigEndian>(jentry)?;
        Ok(vec![OwnedJsonb::new(data)])
    }

    fn build_values(
        root: RawJsonb,
        poses: &mut VecDeque<Position>,
    ) -> Result<Vec<OwnedJsonb>, Error> {
        let mut owned_jsonbs = Vec::with_capacity(poses.len());
        while let Some(pos) = poses.pop_front() {
            let mut data = Vec::new();
            match pos {
                Position::Container((offset, length)) => {
                    data.extend_from_slice(&root.data[offset..offset + length]);
                }
                Position::Scalar((ty, offset, length)) => {
                    data.write_u32::<BigEndian>(SCALAR_CONTAINER_TAG)?;
                    let jentry = ty | length as u32;
                    data.write_u32::<BigEndian>(jentry)?;
                    if length > 0 {
                        data.extend_from_slice(&root.data[offset..offset + length]);
                    }
                }
            }
            owned_jsonbs.push(OwnedJsonb::new(data));
        }
        Ok(owned_jsonbs)
    }

    fn build_scalar_array(
        root: RawJsonb,
        poses: &mut VecDeque<Position>,
    ) -> Result<Vec<OwnedJsonb>, Error> {
        let mut data = Vec::new();
        let len = poses.len();
        let header = ARRAY_CONTAINER_TAG | len as u32;
        // write header.
        data.write_u32::<BigEndian>(header)?;
        let mut jentry_offset = data.len();
        // reserve space for jentry.
        data.resize(jentry_offset + 4 * len, 0);
        while let Some(pos) = poses.pop_front() {
            let jentry = match pos {
                Position::Container((offset, length)) => {
                    data.extend_from_slice(&root.data[offset..offset + length]);
                    CONTAINER_TAG | length as u32
                }
                Position::Scalar((ty, offset, length)) => {
                    if length > 0 {
                        data.extend_from_slice(&root.data[offset..offset + length]);
                    }
                    ty | length as u32
                }
            };
            for (i, b) in jentry.to_be_bytes().iter().enumerate() {
                data[jentry_offset + i] = *b;
            }
            jentry_offset += 4;
        }
        Ok(vec![OwnedJsonb::new(data)])
    }

    // check and convert index to Array index.
    fn convert_index(index: &Index, length: i32) -> Option<usize> {
        let idx = match index {
            Index::Index(idx) => *idx,
            Index::LastIndex(idx) => length + *idx - 1,
        };
        if idx >= 0 && idx < length {
            Some(idx as usize)
        } else {
            None
        }
    }

    // check and convert slice to Array indices.
    fn convert_slice(start: &Index, end: &Index, length: i32) -> Option<Vec<usize>> {
        let start = match start {
            Index::Index(idx) => *idx,
            Index::LastIndex(idx) => length + *idx - 1,
        };
        let end = match end {
            Index::Index(idx) => *idx,
            Index::LastIndex(idx) => length + *idx - 1,
        };
        if start > end || start >= length || end < 0 {
            None
        } else {
            let start = if start < 0 { 0 } else { start as usize };
            let end = if end >= length {
                (length - 1) as usize
            } else {
                end as usize
            };
            Some((start..=end).collect())
        }
    }

    fn filter_expr(
        &'a self,
        root: RawJsonb,
        pos: &Position,
        expr: &Expr<'a>,
    ) -> Result<bool, Error> {
        match expr {
            Expr::BinaryOp { op, left, right } => match op {
                BinaryOperator::Or => {
                    let lhs = self.filter_expr(root, pos, left)?;
                    let rhs = self.filter_expr(root, pos, right)?;
                    Ok(lhs || rhs)
                }
                BinaryOperator::And => {
                    let lhs = self.filter_expr(root, pos, left)?;
                    let rhs = self.filter_expr(root, pos, right)?;
                    Ok(lhs && rhs)
                }
                _ => {
                    let lhs = self.convert_expr_val(root, pos, *left.clone())?;
                    let rhs = self.convert_expr_val(root, pos, *right.clone())?;
                    let res = self.compare(op, &lhs, &rhs);
                    Ok(res)
                }
            },
            Expr::FilterFunc(filter_expr) => match filter_expr {
                FilterFunc::Exists(paths) => self.eval_exists(root, pos, paths),
                FilterFunc::StartsWith(prefix) => self.eval_starts_with(root, pos, prefix),
            },
            _ => todo!(),
        }
    }

    fn eval_exists(
        &'a self,
        root: RawJsonb,
        pos: &Position,
        paths: &[Path<'a>],
    ) -> Result<bool, Error> {
        let poses = self.find_positions(root, Some(pos), paths)?;
        let res = !poses.is_empty();
        Ok(res)
    }

    fn eval_starts_with(
        &'a self,
        _root: RawJsonb,
        _pos: &Position,
        _prefix: &str,
    ) -> Result<bool, Error> {
        // todo
        Ok(false)
    }

    fn convert_expr_val(
        &'a self,
        root: RawJsonb,
        pos: &Position,
        expr: Expr<'a>,
    ) -> Result<ExprValue<'a>, Error> {
        match expr {
            Expr::Value(value) => Ok(ExprValue::Value(value.clone())),
            Expr::Paths(paths) => {
                // get value from path and convert to `ExprValue`.
                let mut poses = VecDeque::new();
                if let Some(Path::Current) = paths.first() {
                    poses.push_back(pos.clone());
                } else {
                    poses.push_back(Position::Container((0, root.len())));
                }

                for path in paths.iter().skip(1) {
                    match path {
                        &Path::Root
                        | &Path::Current
                        | &Path::FilterExpr(_)
                        | &Path::Predicate(_) => unreachable!(),
                        _ => {
                            let len = poses.len();
                            for _ in 0..len {
                                let pos = poses.pop_front().unwrap();
                                match pos {
                                    Position::Container((offset, length)) => {
                                        self.select_path(root, offset, length, path, &mut poses)?;
                                    }
                                    Position::Scalar(_) => {
                                        // In lax mode, bracket wildcard allow Scalar value.
                                        if path == &Path::BracketWildcard {
                                            poses.push_back(pos);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                let mut values = Vec::with_capacity(poses.len());
                while let Some(pos) = poses.pop_front() {
                    if let Position::Scalar((ty, offset, length)) = pos {
                        let value = match ty {
                            NULL_TAG => PathValue::Null,
                            TRUE_TAG => PathValue::Boolean(true),
                            FALSE_TAG => PathValue::Boolean(false),
                            NUMBER_TAG => {
                                let n = Number::decode(&root.data[offset..offset + length])?;
                                PathValue::Number(n)
                            }
                            STRING_TAG => {
                                let v = &root.data[offset..offset + length];
                                PathValue::String(Cow::Owned(unsafe {
                                    String::from_utf8_unchecked(v.to_vec())
                                }))
                            }
                            _ => unreachable!(),
                        };
                        values.push(value);
                    }
                }
                Ok(ExprValue::Values(values))
            }
            _ => unreachable!(),
        }
    }

    fn compare(&'a self, op: &BinaryOperator, lhs: &ExprValue<'a>, rhs: &ExprValue<'a>) -> bool {
        match (lhs, rhs) {
            (ExprValue::Value(lhs), ExprValue::Value(rhs)) => {
                self.compare_value(op, *lhs.clone(), *rhs.clone())
            }
            (ExprValue::Values(lhses), ExprValue::Value(rhs)) => {
                for lhs in lhses.iter() {
                    if self.compare_value(op, lhs.clone(), *rhs.clone()) {
                        return true;
                    }
                }
                false
            }
            (ExprValue::Value(lhs), ExprValue::Values(rhses)) => {
                for rhs in rhses.iter() {
                    if self.compare_value(op, *lhs.clone(), rhs.clone()) {
                        return true;
                    }
                }
                false
            }
            (ExprValue::Values(lhses), ExprValue::Values(rhses)) => {
                for lhs in lhses.iter() {
                    for rhs in rhses.iter() {
                        if self.compare_value(op, lhs.clone(), rhs.clone()) {
                            return true;
                        }
                    }
                }
                false
            }
        }
    }

    fn compare_value(
        &'a self,
        op: &BinaryOperator,
        lhs: PathValue<'a>,
        rhs: PathValue<'a>,
    ) -> bool {
        let order = lhs.partial_cmp(&rhs);
        if let Some(order) = order {
            match op {
                BinaryOperator::Eq => order == Ordering::Equal,
                BinaryOperator::NotEq => order != Ordering::Equal,
                BinaryOperator::Lt => order == Ordering::Less,
                BinaryOperator::Lte => order == Ordering::Equal || order == Ordering::Less,
                BinaryOperator::Gt => order == Ordering::Greater,
                BinaryOperator::Gte => order == Ordering::Equal || order == Ordering::Greater,
                _ => unreachable!(),
            }
        } else {
            false
        }
    }
}

fn decode_header(input: &[u8]) -> IResult<&[u8], (u32, usize)> {
    map(be_u32, |header| {
        (
            header & CONTAINER_HEADER_TYPE_MASK,
            (header & CONTAINER_HEADER_LEN_MASK) as usize,
        )
    })(input)
}

fn decode_jentry(input: &[u8]) -> IResult<&[u8], (u32, usize)> {
    map(be_u32, |jentry| {
        (
            jentry & JENTRY_TYPE_MASK,
            (jentry & JENTRY_OFF_LEN_MASK) as usize,
        )
    })(input)
}

fn decode_jentries(input: &[u8], length: usize) -> IResult<&[u8], Vec<(u32, usize)>> {
    count(decode_jentry, length)(input)
}

fn decode_string(input: &[u8], length: usize) -> IResult<&[u8], &[u8]> {
    take(length)(input)
}
