//! Bounded exact decimal metadata and comparison with decoded IEEE754 values.
//! No product-specific unit, fill value, interval or portrayal policy.
use anyhow::{ensure,Result};
use num_bigint::BigInt;
use std::cmp::Ordering;
/// Canonical finite decimal rational. Text budgets bound work and memory,
/// rather than rounding precision or claiming a universal S100 Real limit.
#[derive(Debug,Clone,Copy)]
enum BinaryCut {Unknown,Exact(u64),Between(u64,u64),AboveFinite,BelowFinite}
#[derive(Debug,Clone)]
pub struct ExactDecimal {numerator:BigInt,denominator:BigInt,cut:BinaryCut}
// Comparison cache is an implementation detail, not part of numeric identity.
impl PartialEq for ExactDecimal {fn eq(&self,other:&Self)->bool {self.numerator==other.numerator && self.denominator==other.denominator}}
impl Eq for ExactDecimal {}
impl ExactDecimal {
 pub fn parse(text:&str)->Result<Self> {
  ensure!(!text.is_empty() && text.len()<=4096 && text.is_ascii(),"Unsupported exact decimal text size/encoding");
  let (negative,body)=if let Some(v)=text.strip_prefix('-'){(true,v)}else{(false,text.strip_prefix('+').unwrap_or(text))};
  let mut parts=body.split(['e','E']);let mantissa=parts.next().unwrap();let exponent=if let Some(v)=parts.next() {
   ensure!(!v.is_empty() && v.len()<=6,"Unsupported exact decimal exponent");
   let digits=v.strip_prefix(['+','-']).unwrap_or(v);ensure!(!digits.is_empty() && digits.bytes().all(|b|b.is_ascii_digit()),"Invalid exact decimal exponent");
   let e=v.parse::<i32>()?;ensure!(e.abs()<=4096,"Unsupported exact decimal exponent magnitude");e
  }else{0};ensure!(parts.next().is_none(),"Repeated decimal exponent");
  let mut dots=mantissa.split('.');let whole=dots.next().unwrap();let fraction=dots.next().unwrap_or("");ensure!(dots.next().is_none(),"Repeated decimal point");
  ensure!(!whole.is_empty() || !fraction.is_empty(),"Missing decimal digits");ensure!(whole.bytes().chain(fraction.bytes()).all(|b|b.is_ascii_digit()),"Invalid exact decimal digits");
  let mut digits=String::with_capacity(whole.len()+fraction.len());digits.push_str(whole);digits.push_str(fraction);
  let mut scale=exponent-fraction.len() as i32;
  while digits.ends_with('0'){digits.pop();scale+=1;}
  if digits.is_empty() {return Ok(Self{numerator:BigInt::from(0),denominator:BigInt::from(1),cut:BinaryCut::Exact(0)})}
  ensure!(scale.abs()<=8192,"Unsupported exact decimal scale");
  let mut n=BigInt::parse_bytes(digits.as_bytes(),10).ok_or_else(||anyhow::anyhow!("Invalid exact decimal coefficient"))?;
  if negative {n=-n;}
  let power=BigInt::from(10u8).pow(scale.unsigned_abs());
  let mut result=if scale>=0 {Self{numerator:n*power,denominator:BigInt::from(1),cut:BinaryCut::Unknown}}else{Self{numerator:n,denominator:power,cut:BinaryCut::Unknown}};
  // A floating parser gives only a guess. Exact BigInt comparisons certify
  // its equality/bracket before the fast path is stored. No assumed rounding.
  if let Ok(guess)=text.parse::<f64>() {
   if guess.is_finite() {
    result.cut=match result.compare_binary64_slow(guess)? {
     Ordering::Equal=>BinaryCut::Exact(if guess==0. {0}else{guess.to_bits()}),
     Ordering::Greater=>{let upper=guess.next_up();if upper.is_finite() && result.compare_binary64_slow(upper)?==Ordering::Less {BinaryCut::Between(guess.to_bits(),upper.to_bits())}else{BinaryCut::Unknown}},
     Ordering::Less=>{let lower=guess.next_down();if lower.is_finite() && result.compare_binary64_slow(lower)?==Ordering::Greater {BinaryCut::Between(lower.to_bits(),guess.to_bits())}else{BinaryCut::Unknown}},
    };
   } else if guess==f64::INFINITY && result.compare_binary64_slow(f64::MAX)?==Ordering::Greater {result.cut=BinaryCut::AboveFinite;}
   else if guess==f64::NEG_INFINITY && result.compare_binary64_slow(-f64::MAX)?==Ordering::Less {result.cut=BinaryCut::BelowFinite;}
  }
  Ok(result)
 }
 pub fn compare(&self,other:&Self)->Ordering {(&self.numerator*&other.denominator).cmp(&(&other.numerator*&self.denominator))}
 /// Exact comparison with the finite decoded binary number, including subnormals.
 pub fn compare_binary64(&self,value:f64)->Result<Ordering> {
  ensure!(value.is_finite(),"Exact decimal comparison requires finite binary64");
  match self.cut {
   BinaryCut::Exact(bits)=>Ok(f64::from_bits(bits).partial_cmp(&value).unwrap()),
   BinaryCut::Between(lo,_) if value<=f64::from_bits(lo)=>Ok(Ordering::Greater),
   BinaryCut::Between(_,hi) if value>=f64::from_bits(hi)=>Ok(Ordering::Less),
   BinaryCut::AboveFinite=>Ok(Ordering::Greater),BinaryCut::BelowFinite=>Ok(Ordering::Less),
   _=>self.compare_binary64_slow(value),
  }
 }
 fn compare_binary64_slow(&self,value:f64)->Result<Ordering> {
  ensure!(value.is_finite(),"Exact decimal comparison requires finite binary64");
  let bits=value.to_bits();let exponent=((bits>>52)&0x7ff) as i32;let fraction=bits&((1u64<<52)-1);
  let (mantissa,power)=if exponent==0 {(fraction,-1074)}else{((1u64<<52)|fraction,exponent-1023-52)};
  let mut n=BigInt::from(mantissa);if bits>>63!=0 {n=-n;}
  let (n,d)=if power>=0 {(n<<power as usize,BigInt::from(1))}else{(n,BigInt::from(1)<<(-power) as usize)};
  Ok((&self.numerator*d).cmp(&(n*&self.denominator)))
 }
}
#[cfg(test)] mod tests {
 use super::*;
 #[test] fn exact_decimal_distinguishes_adjacent_lexical_endpoints() {
  let a=ExactDecimal::parse("1.0000000000000000001").unwrap();assert_eq!(a.compare_binary64(1.).unwrap(),Ordering::Greater);
  let b=ExactDecimal::parse("1.0000000000000000002").unwrap();assert_eq!(a.compare(&b),Ordering::Less);
  assert_eq!(ExactDecimal::parse("1.50").unwrap(),ExactDecimal::parse("+150e-2").unwrap());
  assert_eq!(ExactDecimal::parse("-0.0").unwrap(),ExactDecimal::parse("0").unwrap());
 }
 #[test] fn exact_decimal_admission_and_extreme_exponents_have_bounded_work() {
  for bad in [""," ","NaN","inf","1e","1e1e2","1..2","--1","1e999999999","1e4097","1e-4097"] {assert!(ExactDecimal::parse(bad).is_err(),"{bad}");}
  assert!(ExactDecimal::parse(&"1".repeat(4097)).is_err());
  assert_eq!(ExactDecimal::parse("1e4096").unwrap().compare_binary64(f64::MAX).unwrap(),Ordering::Greater);
  assert_eq!(ExactDecimal::parse("1e-4096").unwrap().compare_binary64(f64::from_bits(1)).unwrap(),Ordering::Less);
  assert!(ExactDecimal::parse("1").unwrap().compare_binary64(f64::INFINITY).is_err());
 }
 #[test] fn certified_binary_cut_avoids_repeated_bigint_work_without_rounding_assumption() {
  for s in ["0","1","0.1","-0.1","1.0000000000000000001","-1.0000000000000000001","1e4096","1e-4096","-1e4096","-1e-4096"] {
   let n=ExactDecimal::parse(s).unwrap();assert!(!matches!(n.cut,BinaryCut::Unknown),"{s}");
   for v in [-f64::MAX,-1.,-f64::from_bits(1),-0.,0.,f64::from_bits(1),1.,f64::MAX] {assert_eq!(n.compare_binary64(v).unwrap(),n.compare_binary64_slow(v).unwrap());}
  }
 }
 #[test] fn binary_comparison_matches_independent_python_fraction_oracle() {
  let mut count=0;
  for line in include_str!("test-data/exact_decimal_fraction_oracle.tsv").lines() {
   let mut p=line.split('\t');let decimal=p.next().unwrap();let bits=u64::from_str_radix(p.next().unwrap(),16).unwrap();let expected:i8=p.next().unwrap().parse().unwrap();assert!(p.next().is_none());
   let parsed=ExactDecimal::parse(decimal).unwrap();let value=f64::from_bits(bits);let got=parsed.compare_binary64(value).unwrap();assert_eq!(got,parsed.compare_binary64_slow(value).unwrap());let sign=match got {Ordering::Less=>-1,Ordering::Equal=>0,Ordering::Greater=>1};assert_eq!(sign,expected,"{decimal} {bits:x}");count+=1;
  }
  assert_eq!(count,1024);
 }
}
