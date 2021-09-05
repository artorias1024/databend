// Copyright 2020 Datafuse Labs.
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

use std::fmt;

use common_datavalues::chrono::Utc;
use common_datavalues::prelude::*;
use common_exception::Result;

use crate::scalars::Function;

#[derive(Clone)]
pub struct TimeSlotFunction {
    display_name: String,
}

impl TimeSlotFunction {
    pub fn try_create(display_name: &str) -> Result<Box<dyn Function>> {
        Ok(Box::new(TimeSlotFunction {
            display_name: display_name.to_string(),
        }))
    }
}

impl Function for TimeSlotFunction {
    fn name(&self) -> &str {
        self.display_name.as_str()
    }

    fn return_type(&self, _args: &[DataType]) -> Result<DataType> {
        Ok(DataType::DateTime32)
    }

    fn nullable(&self, _input_schema: &DataSchema) -> Result<bool> {
        Ok(false)
    }

    fn eval(&self, columns: &DataColumnsWithField, _input_rows: usize) -> Result<DataColumn> {
        let timestamp = Utc::now().timestamp_millis() / 1000;
        let mut times = vec![DataValue::Int64(Some(timestamp))];
        let len = columns.len();
        if len > 0 {
            times = columns[0].column().to_values()?;
        }

        let mut result: Vec<u32> = Vec::new();
        for val in times {
            let re_val = slot_time(val.as_i64()?) as u32;
            result.push(re_val);
        }
        Ok(DataColumn::Array(Series::new(result)))
    }

    fn variadic_arguments(&self) -> Option<(usize, usize)> {
        Some((0, 1))
    }
}

fn slot_time(timestamp: i64) -> i64 {
    let mut minutes = ((timestamp / 60) % (60 * 24)) % 60;
    if minutes > 30 {
        minutes -= 30;
    }
    timestamp - minutes * 60
}

impl fmt::Display for TimeSlotFunction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "timeSlot")
    }
}
