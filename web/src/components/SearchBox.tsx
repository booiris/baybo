import type { InputHTMLAttributes } from 'react';
import { RiFilter3Line } from 'react-icons/ri';

export function SearchBox({ className = '', ...rest }: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <div
      className={`flex-1 flex items-center gap-3 px-4 border-2 border-black rounded-md bg-white ${className}`}
    >
      <RiFilter3Line className="text-ink-soft" />
      <input
        type="text"
        className="w-full border-none outline-none py-3 bg-transparent font-mono text-[0.95rem]"
        {...rest}
      />
    </div>
  );
}
