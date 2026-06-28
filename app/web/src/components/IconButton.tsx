import type { ButtonHTMLAttributes, ReactNode } from 'react';

interface IconButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  children: ReactNode;
}

export function IconButton({ className = '', children, ...rest }: IconButtonProps) {
  return (
    <button
      className={`inline-flex items-center justify-center w-9 h-9 bg-transparent border-2 border-black rounded-md shadow-brutal-xs cursor-pointer transition-[transform,box-shadow] duration-100 active:translate-x-[2px] active:translate-y-[2px] active:shadow-none ${className}`}
      {...rest}
    >
      {children}
    </button>
  );
}
